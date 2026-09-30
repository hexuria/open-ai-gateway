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
| xAI | `xai` (`grok`) | OpenAI Chat Completions | `api_key`, `oauth` | **Yes.** `oag admin account add --from grok` imports every signed-in Grok CLI session and requests route through it. A seat serves the one principal named by `--owner-email` and nobody else. |
| AWS Bedrock | `bedrock` | Anthropic Messages | `api_key` (SigV4 key, packed) | Not a subscription product. |
| Jev (TypeSafe AI) | `jev` (`typesafe`) | System One | `api_key` | No importer. Serves System One only, never chat — see [System One](#system-one-jev). |

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
`--priority` (0) and `--max-concurrency` (8 for an API key, 2 for an imported seat). Bedrock is the only shape
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

`--owner-email` is **required**. A subscription is sanctioned for its holder's
own use, so a seat belongs to one principal and is never pooled; one person may
own several — see [compliance.md](compliance.md). `--monthly-cost` records
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
qualifiers, and `bedrock` has none because nothing can address it a second
way.

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
    async fn prepare_credential<'a>(&'a self, account: AccountId, stored: &'a SecretMaterial)
        -> Result<Cow<'a, SecretMaterial>>;
}
```

Deliberately narrow. Everything an adapter does *not* need to know — which
credential to use, whether to retry, what it cost — is decided before it is
called. The trait is the shape: adding a provider means implementing it.

`parse_event` returns a `Vec` because the mapping is not one-to-one: an
Anthropic `content_block_start` plus its deltas is a single OpenAI chunk, and
one OpenAI chunk carrying both content and a tool call is two canonical events.
Returning an empty vec is normal — most dialects emit bookkeeping lines that
carry nothing.

`refresh` defaults to "nothing to do", which is correct for every static API key,
so only OAuth-style adapters implement it.

`prepare_credential` turns the stored credential into the one a request is
built with. It defaults to the stored one, unchanged and uncopied; an adapter
overrides it where what is stored is not what goes on the wire, such as a
service account's JSON key exchanged for a short-lived token. The request path
calls it once per credential tried, after `refresh` and before `build`, and an
error from it is that credential failing: the request moves to the next one.

## Most providers need no adapter

`Provider::native_dialect` maps a provider to the wire format it speaks. OpenAI,
Kimi, DeepSeek, Zhipu, and xAI all speak Chat Completions, so **one adapter
covers all five** — `OpenAICompatAdapter` — and they differ only in base URL and
catalog entries. Check the dialect before writing code.

So for anything that speaks Chat Completions, the work is a catalog entry and a
base URL rather than an adapter. A vendor nobody has named does not even need
that much code: register it as an endpoint (below), and it is served by the
adapter for its dialect under a name of your choosing.

Point any of them somewhere else without a rebuild:

```yaml
gateway:
  provider_base_urls:
    kimi: "https://your-proxy.internal/v1"
```

## Registered endpoints

An endpoint is an upstream you register instead of one built in. It has a name,
which is also its models' prefix (`groq/llama-…`); a dialect (`openai`,
`anthropic`, `gemini` or `system_one`); a platform (`plain`, `azure`, `aws` or
`gcp`); and its settings, all in the `endpoint` table (migration 0020). Its keys
are ordinary sealed credentials filed under its name
(`oag admin account add --provider groq`, which files the one kind the
endpoint's platform takes), and its models are catalog rows whose provider is
its name.

The gateway reads the table on every catalog refresh
(`gateway.catalog_refresh_interval`, or `POST /admin/api/catalog/reload`), and
reads it before the catalog, so an endpoint, its keys and its models written
together are served within one refresh and without a restart. Each row has to
pass these rules, which `account add` also applies (all but the header names)
before it files a key under an endpoint's name:

- the name is 1 to 32 of `a-z`, `0-9`, `_` and `-`, and is not a built-in
  provider's name or alias, `oag` or `codex`;
- the platform serves the dialect: `plain` all four, `azure` `openai`, `aws`
  `anthropic`, `gcp` `gemini` and `anthropic`;
- the auth style is how the platform takes a key: any of them on `plain`,
  `api_key_header` on `azure`, `bearer` on `gcp` (a minted token) and `none` on
  `aws` (the request is signed instead);
- `plain` and `azure` have a base URL: http or https, with no credentials,
  query or fragment in it, no link-local or cloud-metadata address, and on
  `plain` none of the hosts [compliance.md](compliance.md#a-plain-endpoint-cannot-reach-a-providers-own-api)
  lists. Loopback and private addresses are allowed, for a model server on your
  own network;
- a region (required on `aws` and `gcp`) and a project (required on `gcp`) are
  1 to 63 of `a-z`, `0-9` and `-`, because a platform puts them in a hostname or
  a path;
- extra headers are strings, and none of them is `authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key`, `cookie`, `host`, `content-length` or `proxy-*`
  (`oag_core::endpoint` checks the strings; the names are checked where the
  gateway turns them into headers).

A row that breaks one is skipped, and the rest are served as before. Its keys
and models serve nothing, a warning naming it is logged on every refresh, and
`oag_endpoint_invalid_total{reason}` counts it. In this release only `plain`
endpoints speaking `openai`, `anthropic` or `gemini` are served; an `azure`,
`aws` or `gcp` endpoint, or a `plain` `system_one` one, is a valid row skipped
with reason `unsupported` until its adapter lands.

A request already sent when its endpoint's settings change is not moved: it was
built for the old base URL and headers and is answered from there, and the next
request gets the new settings. A request in flight when its endpoint is removed
can fail once its answer arrives, so remove an endpoint's keys, and let their
requests finish, before the endpoint itself.

### An endpoint's models

An endpoint's models are ordinary catalog rows, `<endpoint>/<model>`, where
`<model>` is the name the endpoint takes on the wire and may hold slashes of its
own: `merge/zai/glm-5.3-flash` sends `zai/glm-5.3-flash`, everything after the
first slash. Two things read the endpoint's own model list, each with one of
its keys, in the header its auth style names and with its extra headers, and
neither follows a redirect.

**`oag admin endpoint sync <name>`** writes the list into the catalog, when the
list prices its models:

```sh
oag admin endpoint sync merge --dry-run     # what would change; writes nothing
oag admin endpoint sync merge               # write it
oag admin endpoint sync merge --include 'anthropic/*' --exclude '*-preview' --price first
```

- **Where it looks.** At `--listing-url` if given: on the base URL's own origin,
  because the key goes with the request, and used for that run only, stored
  nowhere. Otherwise at `{base}/models` (`{base}/v1/models` for an `anthropic`
  endpoint), and then at `/v1/models` on the base URL's origin, which is where
  Merge Gateway keeps the priced list for every surface it serves: an endpoint
  based at `https://api-gateway.merge.dev/v1/openai` is synced from
  `https://api-gateway.merge.dev/v1/models`. Each request asks for `limit=500`
  unless the URL names a limit, and the list is followed across `has_more` /
  `next_cursor` pages, twenty at most. A list read only in part is not used.
- **What it reads.** Merge's shape: each entry's `model`, `display_name`,
  `availability_status` and `access_required`, and each vendor's
  `context_window`, `max_output_tokens`, `availability_status`, `capabilities`
  and `pricing`. An id-only list, such as OpenAI's `/models`, is refused and
  nothing is written, because a catalog row needs a price and none is ever
  invented: add those models one at a time with `oag admin catalog add`. Lists
  shaped like OpenRouter's or LiteLLM's are not read.
- **What it keeps.** Chat models: a vendor whose input and output both include
  `text`, that is not marked deprecated, unavailable, retired, disabled,
  discontinued or sunset, does not need access (`access_required`), and states
  both per-token prices, not both zero. A model is priced by the cheapest such
  vendor, input plus output and the first listed on a tie (`--price cheapest`,
  the default), because that is where Merge sends it; or by the first listed
  (`--price first`). The window and the capabilities come from the same vendor:
  tools from `supports_tool_calling`, reasoning from `supports_reasoning`,
  vision when the input includes `image`, and a prompt cache when a cache-read
  price is stated.
- **What it writes.** Each kept model's row, as an override, so `catalog seed`
  and `catalog sync-prices` never touch it, and rewritten by the next sync when
  the list changes: the list owns the numbers of the rows it prices, so a price
  edited by hand on one of them lasts until the next sync, unless that model is
  `--exclude`d. A label, `<display_name> (<endpoint display name>)`, is written
  only where the row has none, so a name you gave a model survives. A row of the
  endpoint's that the list no longer offers is removed, unless a route's ladder
  names it: then it is kept and reported. `--include` and `--exclude` globs
  (`*` and `?`, matched against the upstream id or the catalog id) scope what a
  run manages, and a model they leave out is neither written nor removed. A list
  that offers nothing at all is refused rather than read as "remove every
  model". The whole sync is one transaction.
- **What it prints.** Added, updated, unchanged, removed and kept counts,
  skipped entries by reason, and what the filters left out. The running gateway
  serves the rows from its next catalog refresh
  (`gateway.catalog_refresh_interval`); nothing restarts.

**Discovery.** With the endpoint's `discover_models` set, the usage poller
(`gateway.usage_poll_interval`) asks each schedulable API key filed under the
endpoint for the list at `{base}/models` (`{base}/v1/models`, with
`anthropic-version`, for `anthropic`; `models[].name` less its `models/` for
`gemini`), across its pages (`next_cursor`, `last_id` or `nextPageToken`), and
records the ids in the key's `served_models`. `/v1/models` then lists only the
endpoint's catalog rows that some key serves. Each key is read by one replica
per interval, under the claim a seat's quota read takes. A failed read changes
nothing; an empty list is recorded as empty and hides the endpoint's models;
turning discovery off forgets what it recorded, so the listing goes back to
every catalog row the endpoint has. Discovery reads only the dialect's own list
URL, never the origin's: a served set read from some other service's list would
hide the endpoint's models. It never writes a catalog row, because it has no
prices. `oag admin endpoint models <name>` prints what it would record, beside
the catalog, and writes nothing.

## Which dialect reaches which upstream

Any inbound dialect can reach any upstream one; translation goes through the
canonical form. When the two agree, bytes pass through **verbatim** — the
upstream's own bytes are the most faithful answer available, and re-serialising
can only differ from them.

All four chat dialects parse inbound and render outbound, so any chat client
shape reaches any chat upstream shape. System One is the exception: it is not a
conversation, so nothing translates into or out of it.

| Dialect | Inbound route | Renders outbound |
|---|---|---|
| Anthropic Messages | `/v1/messages` | yes |
| OpenAI Chat Completions | `/v1/chat/completions` | yes |
| OpenAI Responses | `/v1/responses` | yes |
| Gemini | `/v1beta/models/{model}:generateContent` | yes |
| System One | `/jev/v1/systemone` | no — passed through to a Jev upstream, and only to one |

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

## System One (Jev)

Jev answers questions rather than continuing a conversation. A `state` and named
`questions` go in — `noul` (yes or no), `choice` or `score` — and typed
`answers` come back, each with its confidence and the whole distribution. The
gateway serves it at `POST /jev/v1/systemone` and `GET /jev/v1/models`, under a
prefix so the unmodified TypeSafe SDK works with its base URL set to
`https://<gateway>/jev`, and so Jev's listing never collides with this
gateway's own `/v1/models`. [08-clients.md](08-clients.md) has the client side.

```sh
oag admin account add --name jev-1 --provider jev --secret <TypeSafe API key> --route default
```

```yaml
gateway:
  provider_base_urls:
    jev: "https://api.typesafe.ai"   # the default; set it for a proxy or a mock
```

**Jev is a `Provider`, not a service beside the enum**, for what that buys with
no new code: its key is a sealed `account` row on a route like any other
credential, and leasing, seat ownership, concurrency slots, the per-credential
breaker, retries, failover between Jev keys and the ledger all key on the
provider. What it does not get is a chat adapter. `Dialect::SystemOne` is the
one dialect the canonical form cannot carry, and no chat request can reach a
Jev key: the catalog chat requests route over never holds a System One model,
so neither its name, its bare upstream name, nor a rung someone wrote it onto
can send chat there. A chat request naming `jev/jev-latest` is refused, and
told where System One is served.

**There is no fallback.** No chat model can answer a System One question, so a
route without a Jev key refuses with 503 `system_one_not_configured` rather than
answering with a guess. A route whose Jev keys exist and are all disabled or
cooling down gets the ordinary 503 `no_credential`.

**What passes through.** The request is checked against the SDK's own wire
types — a body the client would refuse to send is a 400 before any key is
used — and forwarded as it arrived. Jev's answer is checked the same way and
returned byte for byte, with Jev's `x-typesafe-request-id` and the gateway's
`x-oag-model`, `x-oag-request-id` and `x-oag-build` beside it. Failures follow
the chat path's rules: a 408, or a key that cannot be reached, is retried on
the same key before the next is tried; a 429, a 5xx, a key silent past
`upstream_response_timeout`, or a 2xx that is not a System One response moves
to the next key at once (the first three also bench the failing key). A 400,
413 or 422 is about the request, so it comes back to the caller with Jev's body
under `error.upstream`: another key would refuse it too, and there is no
bigger model to climb to.

**Metering.** Every answer is a ledger row under `jev/<the model that
answered>`, with the tokens Jev reported. `jev/jev-latest` is in the built-in
catalog unpriced — TypeSafe publishes no per-token price yet — so rows carry
their tokens at zero cost until someone prices the row (as an override, which a
re-seed leaves alone). A System One row claims no saving: its counterfactual is
its own cost. The route's rate limit admits a System One request as it admits a
chat one. Of the spend caps only the hard stop applies: a budget in its last
fifth (`Constrained`) moves a chat request to a cheaper rung, and System One has
no cheaper rung, so it is served as normal until the cap is exhausted.

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

The seam stays so a different transport can be added later. The default build
links no BoringSSL and ships no TLS-impersonation code. See
[compliance.md](compliance.md).

Transports are pooled per `(credential, proxy)`, not per host. Two credentials
sharing a TCP connection share whatever per-connection state the provider keeps,
so a rate limit on one takes the other down with it. The pool is bounded and
evicts by idle time; an evicted transport's in-flight requests are unaffected,
because the `Arc` outlives the cache entry — a long-running stream is never cut
short by eviction.

**No redirects.** Neither the transport nor the client behind token refresh,
quota polls, model lists and price lookups follows one, because every request
either of them sends carries a credential. `reqwest`, following a redirect to
another host, strips `authorization` and nothing else: `x-api-key`,
`x-goog-api-key`, `api-key` and Bedrock's `x-amz-security-token` went along to
wherever `Location` pointed, and a 307 or 308 posted the body there as well —
a refresh token, on a refresh. So a provider's 3xx is an error. The request
fails without trying another credential, since every one would be sent the same
redirect, and the client gets a 502 `upstream_error` with the provider's status
in `upstream_status`. When a provider moves, point its base URL at where it went.

**System proxy settings.** The build turns on `reqwest`'s `system-proxy`
feature (it comes in with `typesafe-sdk`, and Cargo enables a feature for the
whole binary). On macOS and Windows every upstream client — chat, refresh, usage
poll and Jev alike — therefore honours the operating system's proxy settings
when no other proxy is set. A credential's `proxy_url` still wins; on Linux
nothing changes, and `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` apply as before.

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
