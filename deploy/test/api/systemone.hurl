# System One: Jev's question-answering API, mounted under `/jev` so the
# unmodified TypeSafe SDK works with `base_url = {{host}}/jev`.
#
# Not a conversation. A `state` and named `questions` go up; typed `answers`
# with their confidence come back, byte for byte as Jev sent them. Needs a Jev
# key on the calling key's route:
#
#   oag admin account add --name jev-1 --provider jev --secret <key> --route <route>
#
# Without one every request here is 503 `system_one_not_configured`, which is
# the refusal working rather than the gateway failing.

# ── a yes/no question ──
# `noul` is Jev's yes/no: the answer is the probability of yes.
POST {{host}}/jev/v1/systemone
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "state": "I was charged twice. Please fix this ASAP.",
  "model": "jev-latest",
  "questions": {
    "billing": { "type": "noul", "instructions": "Is this ticket about billing?" }
  }
}
HTTP 200
[Asserts]
jsonpath "$.answers.billing.type" == "noul"
jsonpath "$.answers.billing.noul" isFloat
# Jev's own request id, passed through, beside the gateway's.
header "x-typesafe-request-id" exists
header "x-oag-request-id" exists
header "x-oag-model" startsWith "jev/"

# ── a choice and a score, about structured state ──
# A choice names its labels; a score orders its rubric. Each answer carries its
# confidence and the whole distribution.
POST {{host}}/jev/v1/systemone
Authorization: Bearer {{api_key}}
Content-Type: application/json
{
  "state": { "document": "I was charged twice. Please fix this ASAP." },
  "model": "jev-latest",
  "questions": {
    "tone": {
      "type": "choice",
      "instructions": "What is the customer's tone?",
      "criteria": { "calm": null, "frustrated": null, "angry": null }
    },
    "urgency": {
      "type": "score",
      "instructions": "How urgent is this ticket?",
      "criteria": ["can wait", "this week", "today"]
    }
  }
}
HTTP 200
[Asserts]
jsonpath "$.answers.tone.type" == "choice"
jsonpath "$.answers.tone.confidence" isFloat
jsonpath "$.answers.urgency.type" == "score"
jsonpath "$.usage" exists

# ── the models a Jev key can ask ──
# Jev's own listing, not this gateway's `/v1/models`: the prefix is what keeps
# the two apart.
GET {{host}}/jev/v1/models
Authorization: Bearer {{api_key}}
HTTP 200
[Asserts]
jsonpath "$.models[0].name" exists

# ── a question set with no questions ──
# Refused here, in the gateway's envelope, before a Jev key is used. The SDK
# reads `error.message`, so its caller sees this sentence.
POST {{host}}/jev/v1/systemone
Authorization: Bearer {{api_key}}
Content-Type: application/json
{ "state": "x", "questions": {} }
HTTP 400
[Asserts]
jsonpath "$.error.type" == "invalid_request"
jsonpath "$.error.message" contains "At least one question"

# ── no key ──
# Refused on the head, before the body is read, like every inference route.
POST {{host}}/jev/v1/systemone
Content-Type: application/json
{ "state": "x", "questions": { "q": { "type": "noul" } } }
HTTP 401
