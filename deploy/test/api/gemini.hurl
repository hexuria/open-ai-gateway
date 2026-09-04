# The Gemini dialect. The model AND the action live in the path, separated by a
# colon — `/v1beta/models/{model}:{action}` — which is why the route is a
# wildcard and why the split is on the LAST colon: a model id may contain one.
#
# The id here also contains a slash (`openai/gpt-5.6-luna`), so the wildcard is
# doing two jobs at once.

# ── generateContent ──
POST {{host}}/v1beta/models/oag/default:generateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "contents": [{ "role": "user", "parts": [{ "text": "say hi in three words" }] }]
}
HTTP 200
[Asserts]
bytes count > 0

# ── a system instruction ──
# This dialect's name for the system prompt, and a different shape again from
# the other two: an object with parts, not a string and not a message.
POST {{host}}/v1beta/models/oag/default:generateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "systemInstruction": { "parts": [{ "text": "Answer in exactly one word." }] },
  "contents": [{ "role": "user", "parts": [{ "text": "What colour is the sky on a clear day?" }] }]
}
HTTP 200

# ── a multi-turn conversation ──
# `model` is this dialect's name for the assistant role.
POST {{host}}/v1beta/models/oag/default:generateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "contents": [
    { "role": "user", "parts": [{ "text": "My name is Ada." }] },
    { "role": "model", "parts": [{ "text": "Nice to meet you, Ada." }] },
    { "role": "user", "parts": [{ "text": "What is my name?" }] }
  ]
}
HTTP 200

# ── generationConfig ──
# Where this dialect puts what the others call `max_tokens`.
POST {{host}}/v1beta/models/oag/default:generateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "contents": [{ "role": "user", "parts": [{ "text": "count to five" }] }],
  "generationConfig": { "maxOutputTokens": 64 }
}
HTTP 200

# ── streamGenerateContent ──
POST {{host}}/v1beta/models/oag/default:streamGenerateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "contents": [{ "role": "user", "parts": [{ "text": "count to five" }] }]
}
HTTP 200

# ── a concrete model, slash and all ──
POST {{host}}/v1beta/models/{{model}}:generateContent
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "contents": [{ "role": "user", "parts": [{ "text": "reply with the single word: ok" }] }]
}
HTTP 200
