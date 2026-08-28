# Clients

Every client needs exactly two settings: a base URL and an OAG inference key.
Everything else in this document is a consequence of those two, or a trap that
has cost someone an afternoon.

## The model

A client sends its request to the gateway in whatever dialect it already
speaks. The gateway authenticates the key, decides which model should serve the
request, leases a credential for that model's provider, translates the request
into the provider's dialect, calls the real upstream, and translates the answer
back into the dialect the client sent.

Two consequences worth stating plainly, because they are what makes this
different from putting a provider's own URL in a config file:

- **The client never holds a provider key.** It holds an OAG key. Provider
  credentials live in the gateway, encrypted, and are selected per request. Add
  an API key or import a seat and every client picks it up without being
  reconfigured; revoke one and none of them break.
- **The client's dialect need not match the provider's.** An Anthropic-shaped
  client can be served by xAI, and an OpenAI-shaped client by Anthropic,
  because translation goes through one canonical form rather than pairwise
  converters — see [00-architecture.md](00-architecture.md).

What the gateway accepts on its inference listener:

| Surface | Method and path | Dialect |
|---|---|---|
| Anthropic Messages | `POST /v1/messages` | Anthropic |
| Token preflight | `POST /v1/messages/count_tokens` | Anthropic |
| OpenAI Chat Completions | `POST /v1/chat/completions`, `POST /chat/completions` | OpenAI |
| OpenAI Responses | `POST /v1/responses`, `POST /responses` | OpenAI |
| Gemini | `POST /v1beta/models/{model}:generateContent` | Gemini |
| Discovery | `GET /v1/models`, `GET /models`, `GET /v1beta/models` | OpenAI / Gemini |

The unversioned spellings exist because SDKs disagree about whether a custom
base URL already contains the version prefix; both work, so neither guess is
wrong.

The key may arrive as `Authorization: Bearer <key>`, `x-api-key: <key>`, or
`x-goog-api-key: <key>` — one per SDK family. A `Bearer` authorization wins if
a client sends more than one.

Locally the gateway binds **:29080** for inference and **:29081** for the admin
API and dashboard; `just serve` walks up to the first free pair and prints what
it chose. Two listeners, because an SDK key must not be able to reach admin —
see [01-deployment.md](01-deployment.md).

## Two kinds of key, and the 403 that follows from mixing them

Mint them with the same command and one flag apart:

```sh
# For clients. This is what goes in an SDK config, and it cannot reach admin.
oag admin key create --email you@example.com --name cli

# For the dashboard and /admin/api on the second listener.
oag admin key create --email you@example.com --name admin --admin
```

Pasting an **inference** key into the dashboard returns 403, not 401, with:

> this key was not minted as an admin key; mint one with
> `oag admin key create --admin`. An inference key is deliberately not enough

This is the most common first failure. A 401 there means no key or an unknown
one; a 403 means the key is real and was minted for the wrong listener.

`oag admin key list` shows every key by prefix, name, principal and route.
Keys are shown once at creation and stored hashed, so a lost key is reminted,
not recovered.

## Claude Code

```sh
export ANTHROPIC_BASE_URL=https://gateway.example.com   # or http://127.0.0.1:29080
export ANTHROPIC_AUTH_TOKEN=oag_live_...                # an inference key
```

`ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer`; `ANTHROPIC_API_KEY`
is sent as `X-Api-Key`. Both are accepted, and the auth token wins if both are
set — which is worth knowing, because a stale `ANTHROPIC_API_KEY` in a shell
profile is invisible rather than an error.

Model selection:

| Variable | What it sets |
|---|---|
| `ANTHROPIC_MODEL` | the model for this session |
| `ANTHROPIC_DEFAULT_MODEL` | the default for new sessions |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | the cheap tier — **and all background work** |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | the mid tier |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | the frontier tier |
| `ANTHROPIC_DEFAULT_FABLE_MODEL` | the fable tier |
| `ANTHROPIC_DEFAULT_<TIER>_MODEL_NAME` / `_DESCRIPTION` | how `/model` labels the entry |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | adds one custom entry to `/model` |

`ANTHROPIC_SMALL_FAST_MODEL` is deprecated; use `ANTHROPIC_DEFAULT_HAIKU_MODEL`.
The haiku slot also drives summarisation and title generation, so pointing it
at a frontier model bills frontier prices for housekeeping.

Claude Code does **not** validate model strings against a custom base URL, so
any id the gateway's catalog knows passes through — `xai/grok-4.6`,
`anthropic/claude-haiku-4.5`, `oag/auto`. And note the ceiling on all of this:
**per-tier variables only matter in passthrough mode.** In managed mode the
gateway classifies the request and picks the rung, and the client's model name
is not consulted. See the next section but one.

### Model discovery, and the filter nobody would guess

```sh
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

With this on, the CLI GETs `/v1/models`, caches the answer at
`~/.claude/cache/gateway-models.json` as
`{ baseUrl, fetchedAt, models: [{ id, display_name }] }`, and builds `/model`
from the cache. Two details decide whether you see anything at all:

- it **discards every id not matching `/^(claude|anthropic)/i`**, silently — a
  gateway serving `xai/grok-4.6` and `oag/auto` populates an *empty* picker
  and says nothing;
- it uses the cache only when the cached `baseUrl` is byte-identical to
  `ANTHROPIC_BASE_URL`, and refreshes it only while holding a credential.

So the gateway can advertise each entitled model a second time under
`anthropic/<canonical-id>`, with the readable name in `display_name`:

```yaml
gateway:
  claude_code_model_aliases: true
```

Measured on a live gateway: 4 ids served, 0 survived the filter; with aliases
on, 8 served, 4 survived. Sending an alias back resolves to the canonical id,
and **the ledger records the canonical id**, so one model's costs never split
across two names. Aliases are accepted on inference whether or not the flag is
on — a cache written while it was on must not start failing when it is turned
off.

Off by default, because it doubles the listing for every other client. To see
exactly what the CLI would cache without flipping the flag for everyone:

```sh
curl -s -H "Authorization: Bearer $OAG_KEY" \
  "$ANTHROPIC_BASE_URL/v1/models?claude_code=1" | jq '.data[].id'
```

Full rationale in [03-providers.md](03-providers.md).

### Background traffic

```sh
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
```

Stops telemetry and background calls from billing through the proxy. Worth
setting on a metered route; irrelevant on a seat, which is flat-rate.

`ANTHROPIC_CUSTOM_HEADERS` passes extra headers, newline-separated — the way to
send `x-oag-tier` from a client that gives you no other hook.

## OpenAI SDKs and Codex

```sh
export OPENAI_BASE_URL=https://gateway.example.com
export OPENAI_API_KEY=oag_live_...       # an OAG key, not an OpenAI one
```

```python
from openai import OpenAI

client = OpenAI()  # reads both variables
client.chat.completions.create(
    model="xai/grok-4.6",
    messages=[{"role": "user", "content": "hello"}],
)
```

Both the Chat Completions and the Responses surfaces are served, so an SDK that
defaults to either works unchanged. The `model` string is an OAG catalog id
(`<provider>/<model>`), a virtual name (`oag/auto`, `oag/<rung>`), or — in
managed mode — ignored.

A **Codex seat** and a **Codex client** are different things and are configured
in different places. Importing a seat so the gateway can *serve* from it is
`oag admin account add --from codex`, plus `gateway.codex.instructions`; that
is upstream configuration, covered in [03-providers.md](03-providers.md).
Pointing a client at the gateway is the two settings above and nothing else.

## curl, in both dialects

Anthropic:

```sh
curl -s http://127.0.0.1:29080/v1/messages \
  -H "x-api-key: $OAG_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{
        "model": "anthropic/claude-haiku-4.5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "say hi"}]
      }'
```

OpenAI:

```sh
curl -s http://127.0.0.1:29080/v1/chat/completions \
  -H "Authorization: Bearer $OAG_KEY" \
  -H "content-type: application/json" \
  -d '{
        "model": "xai/grok-4.6",
        "messages": [{"role": "user", "content": "say hi"}]
      }'
```

Add `"stream": true` (Anthropic and OpenAI) for SSE. What the gateway actually
served comes back on the response headers, so use `-i` when you are debugging
routing:

| Header | Meaning |
|---|---|
| `x-oag-model` | the canonical id that served the request |
| `x-oag-tier` | the rung it came from. Omitted when the named model sits on no rung (passthrough of an off-ladder id) — it is not `cheap`. |
| `x-oag-request-id` | the id to look up in the ledger |

What the calling key is entitled to, and in which mode:

```sh
curl -s -H "Authorization: Bearer $OAG_KEY" \
  http://127.0.0.1:29080/v1/models | jq '.oag, [.data[].id]'
```

That listing is per-caller: the intersection of the route's ladder, the key's
floor tier, and the providers that route holds live credentials for
(owner-scoped, so a personal seat appears only for its owner), minus providers
whose credentials are all rate limited, reserved-out, or whose subscription
allowance is spent. A metered API key still offers the rest of that provider's
catalog (off-ladder names `decide` would honour). A subscription-only seat
does not — the catalog is an API menu, not a SuperGrok plan. A key with headroom left (`quota_usd` $30 spent $0.50)
still lists. If the calling key's quota — or the route or principal budget —
is exhausted, `data` is empty, including no `oag/*` names.

`oag.budget.pressure` and `oag.providers` are still present when `data` is
empty, which is how a client tells those causes apart. Each provider the
route holds a credential for (same owner scope as the picker, including seats
that cannot serve right now) has a closed `reason`: `serving`, `reserved`,
`quota_spent`, `rate_limited`, `disabled`, `budget_exhausted`, or
`no_credential`. Numbers and an `until` timestamp come along when they exist.
Operator account names do not — those stay on the admin listener.

## Any OpenAI-compatible tool

The same two settings, wherever the tool keeps them: **base URL** →
the gateway, **API key** → an OAG inference key. If the tool offers a model
list it will populate from `/v1/models`; if it wants a model string typed in,
use a catalog id or `oag/auto`.

If the tool appends its own `/v1`, both spellings are served, so it does not
matter which half of the path you put where — as long as you do not end up with
`/v1/v1`.

## Why did I get a different model than I asked for?

Because the route is in **managed** mode, which is the point of the product and
also the single most surprising thing about it. In managed mode the gateway
classifies the request and picks a rung, **ignoring the model the client
named**. In passthrough mode a named concrete model is honoured. Virtual `oag/*`
names are always managed, whatever the route says.

The worked example, from a real session — both facts one line apart in the same
transcript. The operator launched `claude --model grok-4.6`. The API response's
`model` field said **grok-4.5** (managed mode, tier `balanced`), while the
model's own answer to "what is your model" said **"grok-4.6"**. Earlier the
same setup had the model claim to be "Claude Fable 5" while served by Grok.

**A model's self-report is worthless as evidence.** It only echoes the context
the client injected. The `model` field, the `x-oag-model` header and the ledger
are the only truth.

The ledger's `selection_reason` records which mechanism decided:

| `selection_reason` | What happened |
|---|---|
| `classified` | managed mode chose the rung; the client's model name was not consulted |
| `passthrough` | the client's named model was honoured |
| `floor_pinned` | the key's `--floor-tier` decided it |

Read it on the dashboard's usage view or `GET /admin/api/usage`, matching on the
`x-oag-request-id` from the response.

Two real fixes, depending on whether you want to pin a client or a route:

```sh
# Pin one client: a key whose floor tier is the top rung.
oag admin key create --email you@example.com --name pinned --floor-tier frontier

# Or make the whole route honour named models.
oag admin route mode passthrough --route default
```

The reverse also exists: `x-oag-tier: <rung>` on a request asks for a specific
rung, and it outranks the body's model name — for callers whose request body is
generated by a tool they do not control. It forces managed handling even on a
passthrough route. A rung name that is not on the ladder is *not* an error: it
is logged and the request falls back to classification, deliberately, because
mapping a typo to a rung would silently pin the cheapest one.

## Pinning a channel

The same model can be reachable through several credentials. The default —
unqualified — is the product: the router picks the cheapest live one.

```
xai/grok-4.6         cheapest live credential (the default)
xai/grok-4.6@sub     restricted to a subscription (oauth) credential
xai/grok-4.6@api     restricted to an api_key credential
cursor/gemini-flash  a reseller is a different provider; no qualifier needed
```

Proven: with a real Grok seat and a deliberately invalid xAI API key on one
route, `xai/grok-4.6@sub` answered from the seat while `xai/grok-4.6@api`
returned "Incorrect API key provided". An unknown qualifier is refused naming
the valid ones, and `anthropic/...@sub` is refused because that provider has no
subscription path at all. The prefix and the qualifier compose, so
`anthropic/xai/grok-4.6@sub` — an id a Claude Code picker can produce — is
`xai/grok-4.6` on a subscription. The full convention is in
[03-providers.md](03-providers.md).

## Troubleshooting

`oag admin doctor` is the answer to most of these: it checks migrations,
catalog, route, ladder, accounts, per-rung live credentials and Codex
instructions, exits non-zero on any problem, and prints the command that fixes
it. Run it before reading further.

| Symptom | Likely cause | What to run |
|---|---|---|
| 403 on the dashboard, with "not minted as an admin key" | an inference key in an admin slot | `oag admin key create --email <you> --admin` |
| 401 on the dashboard | no key, or one that does not exist | `oag admin key list` |
| 401 on inference | key revoked, or sent in a header the client mangled | `oag admin key list`, then `curl -i` with `Authorization: Bearer` |
| `no_viable_model` (400) | nothing on this route's ladder can serve the request — no rung, or no credential for the rung's providers | `oag admin doctor --route <route>`; the error itself names the route and the fixing command |
| Empty `/model` picker in Claude Code | the `/^(claude\|anthropic)/i` filter dropped every id | set `gateway.claude_code_model_aliases: true`, then confirm with `/v1/models?claude_code=1` |
| A model you own is missing from `/v1/models` | not on the ladder, below the key's floor tier, owned by another principal, every credential for it is rate limited / reserved-out / spent, or this key's quota is exhausted | `GET /v1/models` `.oag.providers[].reason` and `.oag.budget.pressure`; `oag admin doctor` |
| Wrong model served | managed mode, or a floor tier | see the section above; check `x-oag-model` and `selection_reason` |
| A Codex seat imports, then every request fails | `gateway.codex.instructions` (or `instructions_path`) unset — the backend refuses and it reads as a dead credential | `oag admin doctor` names it; `deploy/codex-instructions.txt` is a starting file |
| `unsupported_field` (400) | the request set a field the chosen upstream's dialect cannot express; refused rather than silently dropped, because a dropped field is indistinguishable from a model ignoring it | drop the field, or pin the request to a provider whose dialect has it |
| 429 with `Retry-After` | the route's rpm limit, or the upstream's own throttle forwarded with the provider's body nested under `error.upstream` | wait the header out; `oag admin account list` shows a parked seat |
| Streaming works locally, 504s in production | a proxy hop between the client and the gateway | [01-deployment.md](01-deployment.md), the seven things that break streaming |

For a gateway that has never served a request, start with
[07-running-locally.md](07-running-locally.md) — the three run modes, and which
key each listener wants.
