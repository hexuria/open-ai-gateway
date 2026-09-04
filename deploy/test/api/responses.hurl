# The OpenAI Responses dialect: `POST /v1/responses`.
#
# What a Codex subscription seat speaks natively, so on this deployment it is the
# shortest path to the upstream — the other two dialects are translated into it.

# ── input as a bare string ──
POST {{host}}/v1/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "input": "say hi in three words"
}
HTTP 200
[Asserts]
bytes count > 0

# ── input as a message list ──
# The richer form, and what a client sends once there is any history.
POST {{host}}/v1/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "input": [
    { "role": "user", "content": "My name is Ada." },
    { "role": "assistant", "content": "Nice to meet you, Ada." },
    { "role": "user", "content": "What is my name?" }
  ]
}
HTTP 200

# ── instructions ──
# This dialect's system prompt. Note the gateway sets its own `instructions` for
# a Codex seat when `gateway.codex.instructions` is configured — the backend
# validates that string against the official client, so a seat with it unset
# reads as a dead credential rather than as a config mistake.
POST {{host}}/v1/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "instructions": "Answer in exactly one word.",
  "input": "What colour is the sky on a clear day?"
}
HTTP 200

# ── streaming ──
POST {{host}}/v1/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "stream": true,
  "input": "count to five"
}
HTTP 200
[Asserts]
body contains "data:"

# ── a concrete model ──
POST {{host}}/v1/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "{{model}}",
  "input": "reply with the single word: ok"
}
HTTP 200

# ── the unversioned path ──
POST {{host}}/responses
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "input": "reply with the single word: ok"
}
HTTP 200
