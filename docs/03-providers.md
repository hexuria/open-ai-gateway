# Providers

Which providers this gateway can hold a credential for, what kind of credential
each one takes, how to register it, and what it takes to add another.

## What each provider supports

Two axes, not one: an API key and a subscription seat are different credentials,
and most providers take one and not the other.

| Provider | Canonical (aliases) | Dialect | Credential | Subscription |
|---|---|---|---|---|
| Anthropic | `anthropic` | Anthropic Messages | `api_key` | **Prohibited.** Anthropic's terms forbid a third party intermediating Claude.ai credentials — see [compliance.md](compliance.md). |
| OpenAI | `openai` | OpenAI Chat Completions | `api_key`, `oauth` | **Yes.** `--from codex` imports a Codex seat, and `CodexAdapter` serves it against the ChatGPT backend. Also needs `gateway.codex.instructions` set, or the backend refuses the request. |
| Google Gemini | `gemini` | Gemini generateContent | `api_key` | No importer. |
| Moonshot Kimi | `kimi` (`moonshot`) | OpenAI Chat Completions | `api_key` | No importer. |
| DeepSeek | `deepseek` | OpenAI Chat Completions | `api_key` | No importer. |
| Zhipu GLM | `zhipu` (`glm`) | OpenAI Chat Completions | `api_key` | No importer. |
| xAI | `xai` (`grok`) | OpenAI Chat Completions | `api_key`, `oauth` | **Yes.** `oag admin account add --from grok` imports every signed-in Grok CLI session and requests route through it. A seat binds to one principal unless `--shared` is passed. |
| AWS Bedrock | `bedrock` | Anthropic Messages | `bedrock` (SigV4) | Not a subscription product. |
| Google Vertex AI | `vertex` | Gemini generateContent | `vertex` (service account) | Not a subscription product. No adapter is registered in this build, so it can be configured and cannot serve. |

Subscription support is three states, not a bool: **served** (the importer ships
and requests route through the seat), **credential-import-only** (the seat
imports, seals and refreshes, and nothing serves inference on it — nothing is in
that state today, and it is reported rather than hidden so an operator who sees
no traffic move is not left debugging it), and **not offered**. "Not offered"
carries a typed reason, because "nobody wrote an importer" and "the provider
forbids it" are different answers to "can this be added?". Anthropic's is the
second: their terms say developers may not collect, store, or intermediate
Claude.ai credentials or session tokens, so there is deliberately no importer to
write. Console API keys pooled for the org are the carve-out those same terms
grant, which is why `api_key` is Anthropic's only kind.
[compliance.md](compliance.md) has the quotes and their sources.

This table is a copy. The original is `Provider::support` in `oag-core`, a total
match over the enum — adding a provider without an entry does not compile.
`oag admin providers` prints a terminal version — provider, dialect, how many
credentials you have registered, and the import command or `no`.
`GET /admin/api/providers` serves the whole structure, typed refusal reasons
included, and the dashboard renders that. Prefer either to this page: they
cannot go stale.

**An alias is an input spelling, not a stored one.** `Provider::from_str`
accepts `moonshot`, `glm` and `grok`, and `account add` stores the canonical
name, so the CLI path is safe. A row written any other way — hand-rolled SQL, a
restore from an older dump — keeps the alias, and the two sides then disagree:
`/v1/models` parses `a.provider` and so advertises the credential, while the
scheduler's candidate query matches `a.provider` against the canonical string
and never finds it. The model is offered and is not reachable, which reads as a
routing bug rather than a bad row. `UPDATE account SET provider = 'kimi' WHERE
provider = 'moonshot'` is the fix.

## Registering a credential

An API key and a subscription seat are different commands, and the seat asks a
question the key does not.

### An API key

```sh
oag admin account add --name deepseek-1 --provider deepseek --secret sk-...
```

`--secret` is read from `OAG_ACCOUNT_SECRET` when it is omitted, so the key need
not appear in shell history or the process table. It is sealed with the KEK
before it reaches the row.

Every `api_key` provider in the table takes exactly that command; only
`--provider` changes. The rest is scheduling — `--route` (`default`),
`--priority` (0) and `--max-concurrency` (8). Bedrock is the only shape
difference, and it is not much of one: its secret is packed as
`access_key:secret[:session_token]`, so it still arrives through `--secret` and
needs no credential shape of its own.

Registering a credential does not call the provider, so a typo in a key is not
caught here. `oag admin doctor` is: it reports every ladder rung whose providers
have no schedulable credential on the route, and prints the `account add` that
fixes it. What it checks is registration and local state — disabled, cooling
down, rate limited — not the upstream's opinion of your key, which arrives on
the first request.

### A subscription seat

```sh
oag admin account add --name grok-seat --from grok --owner-email you@example.com
```

`--from grok` reads `~/.grok/auth.json` and `--from codex` reads
`~/.codex/auth.json`; `--auth-file` overrides the path and is repeatable. The
CLI's file is only ever read — the CLI owns it, and rotated tokens land in the
`account` row instead. Grok imports **every** signed-in session in the file, so
two sessions become `grok-seat-1` and `grok-seat-2`; Codex takes the first file
holding a usable OAuth session, skipping an API-key-only `auth.json`. A session
with no refresh token is imported and says so, because it will die at expiry
rather than rotate.

`--owner-email` or `--shared` is **required**, with no default. A subscription
is sanctioned for its holder's own use, so binding it to one principal is the
assumption and pooling it is a decision someone makes on purpose rather than by
omitting a flag — see [compliance.md](compliance.md). `--monthly-cost` records
the seat's flat price, which is what lets the dashboard net a subscription
against the metered spend it displaced.

### Codex needs one more thing

Importing the seat is half of it. The `chatgpt.com` backend validates the
request's `instructions` against what the official Codex client sends, and this
gateway compiles no copy of that string in. A seat imported without it passes
the client's own system prompt through, the backend refuses the request, and it
looks exactly like a dead credential.

```yaml
gateway:
  codex:
    instructions_path: deploy/codex-instructions.txt
    user_agent: "codex_cli_rs/0.147.0"
```

`deploy/codex-instructions.txt` is a current copy taken from the installed
Codex/opencodex catalog. Keep it in lockstep with the client version; a stale
string is the same refusal. `oag admin doctor` fails when an OpenAI `oauth`
account is attached and neither `instructions` nor `instructions_path` is set,
because that is the misconfiguration whose symptom points at the wrong thing.

### What a plan will actually serve, and reading the refusal

Measured against a free ChatGPT plan: a free plan can serve some models, and the
status code is the whole answer.

| Code | Means |
|---|---|
| 400 | The credential was accepted and *that model* is gated. `gpt-5-codex` returns "not supported when using Codex with a ChatGPT account". |
| 401 | The credential is bad. Re-import the seat. |
| 429 | Valid, entitled, out of quota — `gpt-5.6-luna` returned `usage_limit_reached`. |

The 429 is the informative one: a backend only meters a request it has accepted
as valid and entitled, so a quota error proves both the seat and the plan's
entitlement to that model. OAG classifies it as `RateLimited`, parks the seat
with `rate_limited_until` and attempts failover. Do not read a 400 as a broken
seat or a 429 as a gated model; the two lead in opposite directions.

## Naming a model

    <provider>/<model>[@api|@sub]

| id | means |
|---|---|
| `deepseek/deepseek-v3.2` | the model. The router picks the cheapest live credential for it, which is the default and the point. |
| `xai/grok-4.6@sub` | the same model, pinned to a subscription seat. |
| `xai/grok-4.6@api` | the same model, pinned to an API-key credential. |
| `cursor/gemini-flash-3.7` | not a qualifier. A reseller is a different **provider**. |

**A different upstream is a different provider; the same upstream on a different
credential is a qualifier.** Gemini resold by Cursor is a different base URL,
adapter, auth and bill, so it earns a provider id rather than syntax of its own.
`@api` and `@sub` are the entire vocabulary — they are `CredentialKind`'s two
qualifiers, and `bedrock`, `vertex` and `service_account` have none because
nothing can address them a second way.

A qualifier the provider cannot offer is refused rather than dropped:
`gemini/...@sub` is an error naming the kinds that work, because dropping the
pin would send the request to exactly the credential the caller wrote it to
exclude. [02-cost-routing.md](02-cost-routing.md) has the rest of that grammar,
where the listing offers a qualified id, and what the ledger records.

An id is an address; a label is a name. `display_label` is nullable and `NULL`
means derive one — the provider's own display name plus the upstream name, e.g.
`xAI: grok-4.6`. `PATCH /admin/api/models/{id}` sets it, `/v1/models` serves it
as `display_name`, and the dashboard edits it in place. The column is absent
from `upsert_model`'s conflict list, so `catalog seed` and `catalog sync-prices`
can only ever write it on a first insert: an operator's name survives every
refresh, the same way an `is_override`'d price does.

## Adding a provider

Two things: a `ProviderAdapter`, and catalog entries for its models. Check the
dialect first — most of the time the adapter already exists.

## The adapter contract

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync + Debug {
    fn provider(&self) -> Provider;
    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request>;
    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>>;
    async fn refresh(&self, cred: &SecretMaterial) -> Result<Option<SecretMaterial>>;
}
```

Deliberately narrow. Everything an adapter does *not* need to know — which
credential to use, whether to retry, what it cost — is decided before it is
called. sub2api's equivalent is duck-typed across four concrete services with no
interface at all, which is why adding a provider there means reading all four to
work out the shape.

`parse_event` returns a `Vec` because the mapping is not one-to-one: an
Anthropic `content_block_start` plus its deltas is a single OpenAI chunk, and
one OpenAI chunk carrying both content and a tool call is two canonical events.
Returning an empty vec is normal — most dialects emit bookkeeping lines that
carry nothing.

`refresh` defaults to "nothing to do", which is correct for every static API key,
so only OAuth-style adapters implement it.

## Most providers need no adapter

`Provider::native_dialect` maps a provider to the wire format it speaks. OpenAI,
Kimi, DeepSeek, Zhipu, and xAI all speak Chat Completions, so **one adapter
covers all five** — `OpenAICompatAdapter` — and they differ only in base URL and
catalog entries. Check the dialect before writing code.

So for anything that speaks Chat Completions, the work is a catalog entry and a
base URL rather than an adapter. The honest caveat: a vendor nobody has named
yet still needs a `Provider` variant and its `support()` arm, because both are
total matches the compiler enforces — but that is a few lines of data, not a
protocol implementation.

Point any of them somewhere else without a rebuild:

```yaml
gateway:
  provider_base_urls:
    kimi: "https://your-proxy.internal/v1"
```

## Which dialect reaches which upstream

Any inbound dialect can reach any upstream one; translation goes through the
canonical form. When the two agree, bytes pass through **verbatim** — the
upstream's own bytes are the most faithful answer available, and re-serialising
can only differ from them.

All four dialects parse inbound and render outbound, so any client shape
reaches any upstream shape:

| Dialect | Inbound route | Renders outbound |
|---|---|---|
| Anthropic Messages | `/v1/messages` | yes |
| OpenAI Chat Completions | `/v1/chat/completions` | yes |
| OpenAI Responses | `/v1/responses` | yes |
| Gemini | `/v1beta/models/{model}:generateContent` | yes |

`Provider::OpenAI`'s registered adapter still speaks Chat Completions, so an
API-key OpenAI seat takes the passthrough path when the client does too. A
ChatGPT/Codex **subscription** seat is the same provider key but a different
dialect and backend: `CodexAdapter` talks Responses at
`chatgpt.com/backend-api/codex/responses`, and the gateway selects it
per-account when the leased credential is `kind=oauth`. That is a separate
adapter, not a change to the hub, and not a change to
`Provider::native_dialect`.

The Anthropic direction is the harder one: it uses indexed content blocks that
must be explicitly opened and closed, so the renderer tracks the open block and
closes it before opening another. A client that receives a delta for a block it
was never told about drops it silently.

## Framing

Not every provider streams server-sent events. `ProviderAdapter::framing()`
says which one it speaks, and the default is SSE because all but one do:

| Framing | Providers |
|---|---|
| `Sse` | Anthropic, OpenAI (Chat Completions and Codex), Gemini, Kimi, DeepSeek, Zhipu, xAI |
| `AwsEventStream` | Bedrock |

Bedrock streams length-prefixed binary messages whose payload carries the
provider's own event, base64-encoded. A reader that splits on blank lines finds
nothing in one — and the failure is silent: an empty response and zero recorded
usage, with no error anywhere. `eventstream.rs` decodes it.

This also means **a binary-framed upstream can never be passed through**, even
when the dialects match. Bedrock's dialect *is* Anthropic, so dialect alone
would say passthrough and hand an SSE client a binary envelope; `egress_for`
requires SSE framing as well.

## Transport

`Transport` is a trait with exactly one implementation: `reqwest` over rustls.

The seam exists because sub2api needs to impersonate the official CLI's TLS
fingerprint — it routes resold subscription traffic that providers actively try
to detect. This gateway does not, so the default build links no BoringSSL and
ships no impersonation code. See [compliance.md](compliance.md).

Transports are pooled per `(credential, proxy)`, not per host. Two credentials
sharing a TCP connection share whatever per-connection state the provider keeps,
so a rate limit on one takes the other down with it. The pool is bounded and
evicts by idle time; an evicted transport's in-flight requests are unaffected,
because the `Arc` outlives the cache entry — a long-running stream is never cut
short by eviction.

## Providers with their own adapter

| Provider | Why it needs one |
|---|---|
| Anthropic | The canonical dialect. |
| Gemini | Model and mode in the URL path; its own auth header; a genuinely different body shape. |
| Bedrock | Anthropic's body, but the model is in the path, `anthropic_version` replaces the version header, and every request is SigV4-signed. |

`sigv4.rs` is hand-rolled — a few dozen lines against the AWS SDK's several
hundred transitive crates and a second HTTP stack, none of which this gateway
would use for anything else. Bedrock credentials are stored packed as
`access_key:secret[:session_token]`, so Bedrock needs no separate credential
shape from every other provider.

## Catalog entries

```rust
ModelSpec {
    id: ModelId::new("kimi/k2"),          // canonical, provider/name
    provider: Provider::Kimi,
    upstream_name: "moonshot-v1-128k".into(),  // what goes on the wire
    pricing: Pricing { /* per million tokens, as Decimal */ },
    context_window: 128_000,
    max_output_tokens: 8_192,
    capabilities: Capabilities { vision: false, tools: true, .. },
}
```

Canonical ids are distinct from upstream names because Bedrock calls Sonnet
`anthropic.claude-sonnet-4-v1:0` and routing policy should not have to spell
that.

Prices are `Decimal`, never `f64`. They get multiplied by token counts and
summed across millions of rows, and there is no reason to accept binary
floating-point drift on a fixed-point quantity.

Get capabilities right. They are used to *reject* a rung before sending, so a
vision request never reaches a text-only model — a decision that is free to make
correctly and costs a round trip and a 400 to get wrong.

## Claude Code model discovery

`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` makes the CLI GET the gateway's
`/v1/models`, cache it at `~/.claude/cache/gateway-models.json`, and build its
picker from that. Two details of the cache builder decide whether any of this
works:

- It **discards every id that does not match `/^(claude|anthropic)/i`**. Silently.
  A gateway whose ids are `xai/grok-4.6` and `oag/auto` populates an empty
  picker and says nothing.
- It only uses the cache when the cached `baseUrl` is byte-identical to
  `ANTHROPIC_BASE_URL`, and only refreshes it while holding a credential.

So `gateway.claude_code_model_aliases` advertises each entitled model a *second*
time under `anthropic/<canonical-id>` — `xai/grok-4.6` becomes
`anthropic/xai/grok-4.6`, `oag/auto` becomes `anthropic/oag/auto`. A model whose
canonical id already passes the filter is left alone rather than becoming
`anthropic/anthropic/claude-opus-5`. The readable name lives in `display_name`
("xAI: grok-4.6"), and `oag.alias_of` on the twin names the canonical id so a
dashboard does not count one model twice.

Setup:

```yaml
gateway:
  claude_code_model_aliases: true
```

```sh
export ANTHROPIC_BASE_URL=https://gateway.example.com
export ANTHROPIC_AUTH_TOKEN=oag_live_...
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

Off by default: it doubles the listing for every other client, and nobody else
asked for it. `?claude_code=1` forces the aliases on for one call, so you can
`curl` exactly what the CLI would cache without flipping the flag for everyone.

The aliases are **accepted on inference whether or not the flag is on** — a
cache written while it was on must not start failing when it is turned off. An
inbound name is resolved as-is first and only stripped when the full string
names nothing, so the real `anthropic/claude-opus-5` still resolves to itself
and an unknown model is still reported as unknown. The ledger records the
canonical id either way.

## Testing one

Record real request/response pairs as fixtures and assert the round trip through
the canonical hub: lossless for non-streaming, event-equivalent for streaming.
`oag-proto` is pure, so this needs no network and no database, which is what
makes a large corpus practical.
