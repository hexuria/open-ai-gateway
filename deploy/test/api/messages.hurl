# The Anthropic dialect: `POST /v1/messages`, plus its token counter.
#
# The same models as the OpenAI dialect, reached through a different wire format.
# That translation is the hub of this gateway: an Anthropic client can talk to a
# Codex seat and neither end knows.

# ── the simplest thing that works ──
# `max_tokens` is required in this dialect, unlike the OpenAI one.
POST {{host}}/v1/messages
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "messages": [{ "role": "user", "content": "say hi in three words" }]
}
HTTP 200
[Asserts]
bytes count > 0

# ── a system prompt ──
# Top-level `system`, not a message with `role: system`. That difference is one
# of the things the translation layer exists to absorb.
POST {{host}}/v1/messages
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "system": "Answer in exactly one word.",
  "messages": [{ "role": "user", "content": "What colour is the sky on a clear day?" }]
}
HTTP 200

# ── content as blocks rather than a bare string ──
# Both are legal and clients send both.
POST {{host}}/v1/messages
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "messages": [
    {
      "role": "user",
      "content": [{ "type": "text", "text": "reply with the single word: ok" }]
    }
  ]
}
HTTP 200

# ── streaming ──
POST {{host}}/v1/messages
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "stream": true,
  "messages": [{ "role": "user", "content": "count to five" }]
}
HTTP 200
[Asserts]
body contains "data:"

# ── counting tokens ──
# No inference and no spend, so this is the safe first call when all you want to
# know is whether the door is open and the key works.
POST {{host}}/v1/messages/count_tokens
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "messages": [{ "role": "user", "content": "hello" }]
}
HTTP 200
[Asserts]
jsonpath "$.input_tokens" > 0

# The count grows with the prompt. Weak as assertions go, but it catches a
# counter that has been stubbed to a constant, which a fixed expected number
# would not — that would only catch the tokeniser changing.
POST {{host}}/v1/messages/count_tokens
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "messages": [
    { "role": "user", "content": "hello hello hello hello hello hello hello hello hello hello" }
  ]
}
HTTP 200
[Asserts]
jsonpath "$.input_tokens" > 5

# ── counting for a concrete model ──
# Whether the estimate is the model's own tokeniser or ours is reported rather
# than assumed: `oag_estimate` says which, so a caller budgeting against it
# knows how much to trust it.
POST {{host}}/v1/messages/count_tokens
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "{{model}}",
  "messages": [{ "role": "user", "content": "hello" }]
}
HTTP 200
[Asserts]
jsonpath "$.input_tokens" > 0
jsonpath "$.oag_estimate" exists
