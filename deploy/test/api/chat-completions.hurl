# The OpenAI dialect: `POST /v1/chat/completions`.
#
# NOTE ON THE ASSERTIONS. These check status and that a body came back, not its
# JSON shape, and that is deliberate rather than lazy: a Codex subscription seat
# has no non-streaming mode, so an unstreamed request still comes back as SSE
# and `jsonpath` would fail against a correct answer. Assert on what is true for
# every credential this gateway can route to, or the file only passes on the
# deployment it was written against.

# ── the simplest thing that works ──
# `oag/default` is a virtual name: the router picks from the route's ladder
# rather than the caller naming a model.
POST {{host}}/v1/chat/completions
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
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "messages": [
    { "role": "system", "content": "Answer in exactly one word." },
    { "role": "user", "content": "What colour is the sky on a clear day?" }
  ]
}
HTTP 200

# ── a multi-turn conversation ──
# Turn count is one of the signals the classifier reads, so this is also the
# shape that would route differently on a laddered route.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 64,
  "messages": [
    { "role": "user", "content": "My name is Ada." },
    { "role": "assistant", "content": "Nice to meet you, Ada." },
    { "role": "user", "content": "What is my name?" }
  ]
}
HTTP 200

# ── naming a concrete model ──
# On a passthrough route the name is honoured: the gateway does not silently
# redirect a request somebody deliberately routed.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "{{model}}",
  "max_tokens": 32,
  "messages": [{ "role": "user", "content": "reply with the single word: ok" }]
}
HTTP 200

# ── pinning the credential kind ──
# `@sub` says "use the subscription seat, not a metered key". The unqualified
# form is the default and the whole product — several credentials are several
# paths to one model and picking the cheapest live one is the job — so a
# qualifier is for the caller who has a reason to override that.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "{{model}}@sub",
  "max_tokens": 32,
  "messages": [{ "role": "user", "content": "reply with the single word: ok" }]
}
HTTP 200

# ── streaming ──
# The `data:` frames are the point. A client that sets `stream` and gets one
# whole body has a broken proxy in the path, not a broken gateway.
POST {{host}}/v1/chat/completions
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

# ── tools ──
# Three or more tool schemas is what a real agent turn looks like, and it is the
# threshold the classifier treats as agentic.
POST {{host}}/v1/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 128,
  "messages": [{ "role": "user", "content": "What is the weather in Oslo?" }],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Current weather for a city.",
        "parameters": {
          "type": "object",
          "properties": { "city": { "type": "string" } },
          "required": ["city"]
        }
      }
    }
  ]
}
HTTP 200

# ── the unversioned path ──
# Same handler. Present because SDKs given a custom base URL disagree about
# whether to keep the `/v1` prefix, and finding that out in production is worse.
POST {{host}}/chat/completions
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "model": "oag/default",
  "max_tokens": 32,
  "messages": [{ "role": "user", "content": "reply with the single word: ok" }]
}
HTTP 200
