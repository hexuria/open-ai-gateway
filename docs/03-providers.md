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
not appear in shell history or the process table. `--secret-file <path>` reads
it from a file instead, whole, which is how a Google service account's JSON key
arrives ([Vertex endpoints](#vertex-endpoints)). It is sealed with the KEK
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
    async fn prepare_credential<'a>(&'a self, account: AccountId, stored: &'a SecretMaterial,
        proxy: Option<&str>) -> Result<Cow<'a, SecretMaterial>>;
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
`proxy` is the credential's own `proxy_url`, which carries anything the
preparation sends, as it carries the credential's refresh and its requests.

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
`anthropic`, `gemini`, `system_one` or `bedrock_converse`); a platform
(`plain`, `azure`, `aws` or `gcp`); and its settings, all in the `endpoint`
table (migration 0020). Its keys
are ordinary sealed credentials filed under its name
(`oag admin account add --provider groq`, which files the one kind the
endpoint's platform takes), and its models are catalog rows whose provider is
its name.

The gateway reads the table on every catalog refresh
(`gateway.catalog_refresh_interval`, or `POST /admin/api/catalog/reload`), and
reads it before the catalog, so an endpoint, its keys and its models written
together are served within one refresh and without a restart.

### Registering one

A worked example: [Merge Gateway](https://merge.dev), which serves many vendors'
models behind one OpenAI-compatible surface and takes a Bearer key.

```sh
# The endpoint. --auth defaults to the platform's style, bearer on plain.
# A header is optional and stored in the clear: never a key.
oag admin endpoint add --name merge --dialect openai --platform plain \
  --base-url https://api-gateway.merge.dev/v1/openai \
  --header X-Project-Id=<project uuid>

# Its key, sealed like any other: `api_key`, the kind a plain endpoint takes.
# From the environment, so it stays out of shell history.
OAG_ACCOUNT_SECRET=<merge key> oag admin account add --name merge-1 --provider merge

# Is it there, and does the key work? Without a key most hosts answer 401,
# which still says the host is there; --account sends that credential's key.
oag admin endpoint check merge
oag admin endpoint check merge --account merge-1

# Its models, at your contract's prices per million tokens. The provider is
# everything before the FIRST `/`, so a vendor/model name stays whole.
oag admin catalog add --id merge/zai/glm-5.3-flash --upstream zai/glm-5.3-flash \
  --input-per-mtok <usd> --output-per-mtok <usd> --context 128000 --max-output 8192 --tools

# A ladder that uses it. This sets the route's whole ladder; `oag admin route
# show` prints the one it has.
oag admin route tiers --route default cheap=merge/zai/glm-5.3-flash balanced=xai/grok-4.6

oag admin doctor
```

The rest of the verbs: `endpoint list` (every endpoint, with how many
credentials, models and ladder places name it), `endpoint show <name>`,
`endpoint set <name>` (only the flags given change; an empty value clears one;
`--header` adds or replaces and `--unset-header` drops), and `endpoint remove
<name>`, which refuses while a credential or a catalog model still names the
endpoint and prints how to clear each. There is no `--dialect` or `--platform`
on `set`: they are what the endpoint is, so changing either is a remove and an
add. `doctor` asks every endpoint whether this build serves it, whether it has a
credential in rotation and a model in the catalog (each a failure when
missing), and whether a ladder names one of its models (a warning: a request
naming the model still reaches it).

`catalog add` writes a row as an operator override, so a later `catalog seed`
or `catalog sync-prices` leaves it alone, and it works for a built-in
provider's model too. A price of zero in and zero out needs `--free`: the router
ranks by cost, and a model that costs nothing wins every comparison on every
ladder that names it. `catalog seed --from <litellm file>` takes a LiteLLM
provider's models for an endpoint registered under that provider's name
(`groq`, `openrouter`).

The same writes are on the admin API, and the console's Endpoints table sits on
them:

| Route | What |
|---|---|
| `GET /admin/api/endpoints` | List, with the counts and whether this build serves each. |
| `POST /admin/api/endpoints` | Register. The body is the columns; `auth` defaults to the platform's style. |
| `GET /admin/api/endpoints/{name}` | One endpoint. |
| `PATCH /admin/api/endpoints/{name}` | Change settings. A field left out is kept, `null` clears; `dialect`, `platform` and `name` are a 400. |
| `DELETE /admin/api/endpoints/{name}` | Remove. 409, with the counts, while anything names it. |
| `POST /admin/api/endpoints/{name}/check` | Ask it for its models, with no key. 200 whatever it answered. |

A write from the CLI is served from each replica's next refresh. One through the
admin API is served at once by the replica that took it, which reloads, and by
the others from their next refresh.

### The rules a row must pass

Each row has to pass these rules. `endpoint add` and `endpoint set`, and the
admin API, refuse a row that breaks one before it is written, and also resolve
the base URL's name and refuse one that resolves to a link-local or cloud
metadata address. The name is resolved where the write happens, so register a
name only a cluster can resolve (`vllm.models.svc`) from inside it, or through
the admin API. `account add` applies the rules (all but the header names)
before it files a key under an endpoint's name:

- the name is 1 to 32 of `a-z`, `0-9`, `_` and `-`, and is not a built-in
  provider's name or alias, `oag` or `codex`;
- the platform serves the dialect: `plain` `openai`, `anthropic`, `gemini` and
  `system_one`; `azure` `openai`; `aws` `anthropic` and `bedrock_converse`;
  `gcp` `gemini` and `anthropic`;
- the auth style is how the platform takes a key: any of them on `plain`,
  `api_key_header` on `azure`, `bearer` on `gcp` (a minted token) and `none` on
  `aws` (the request is signed instead);
- `plain` and `azure` have a base URL: http or https, with no credentials,
  query or fragment in it, no link-local or cloud-metadata address, and on
  `plain` none of the hosts [compliance.md](compliance.md#a-plain-endpoint-cannot-reach-a-providers-own-api)
  lists. On `plain`, loopback and private addresses are allowed, for a model
  server on your own network. On `azure` the base URL is an Azure resource's and
  nothing more, `https://{resource}.openai.azure.com` or
  `https://{resource}.services.ai.azure.com`, with no path or port; see
  [Azure OpenAI](#azure-openai);
- a region (required on `aws` and `gcp`) and a project (required on `gcp`) are
  1 to 63 of `a-z`, `0-9` and `-`, because a platform puts them in a hostname or
  a path; and an `aws` region is shaped like one, two letters, then words, then
  one digit (`us-east-1`, `us-gov-west-1`), because it is also the scope every
  request is signed for, and AWS answers a signature for a region that does not
  exist as it answers a bad key;
- a path (migration 0021) is set only on a `system_one` endpoint, and is `/`
  and then at most 127 of letters, digits, `/`, `.`, `_` and `-`, in segments
  that are neither empty, `.` nor `..` (the schema checks the characters, the
  gateway the segments too); see [System One hosts](#system-one-hosts);
- an API version on `azure` is a date and `-preview` or nothing
  (`2024-10-21`, `2025-04-01-preview`), because it becomes each request's
  `api-version`; no other platform reads the column, and the schema does not
  check it;
- extra headers are strings, and none of them is `authorization`, `x-api-key`,
  `x-goog-api-key`, `api-key`, `cookie`, `host`, `content-length` or `proxy-*`
  (`oag_core::endpoint` checks the strings; the names are checked where the
  gateway turns them into headers).

A row that breaks one is skipped, and the rest are served as before. Its keys
and models serve nothing, a warning naming it is logged on every refresh, and
`oag_endpoint_invalid_total{reason}` counts it. In this release every platform
is served: a `plain` endpoint speaking `openai`, `anthropic` or `gemini` by the
chat routes, and a `system_one` one by the System One route
([System One hosts](#system-one-hosts)); an `aws` one by the chat routes too
([Bedrock endpoints](#bedrock-endpoints)), and an `azure`
([Azure OpenAI](#azure-openai)) and a `gcp` one
([Vertex endpoints](#vertex-endpoints)) as well. `endpoint list`, `show`,
`doctor` and the console say when a row is not served, and why. `check` asks
only a `plain` endpoint: the clouds list their models on hosts, and with
signatures, of their own.

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

### Bedrock endpoints

The built-in `bedrock` provider is Claude in one region:
`gateway.bedrock_region`, at `provider_base_urls.bedrock` when that is set. An
endpoint on the `aws` platform is Bedrock again, in a region of its own, so a
gateway can hold as many as it has regions to reach — and beyond Claude. The
built-in is unchanged by any of them, and none of them reads its settings.

| Dialect | Models | Bedrock API |
|---|---|---|
| `anthropic` | Claude | `InvokeModel`: `POST /model/{id}/invoke`, `/invoke-with-response-stream`, in Anthropic's body as the built-in sends it |
| `bedrock_converse` | Llama, Mistral, Nova, and every other model `Converse` serves (Claude too) | `Converse`: `POST /model/{id}/converse`, `/converse-stream` |

- **Region**, required, and the endpoint's own: it names the host,
  `https://bedrock-runtime.{region}.amazonaws.com`, and the scope every request
  is signed for. Each endpoint signs for its region, never for
  `gateway.bedrock_region`.
- **Base URL**, optional: set it for a VPC interface endpoint, a proxy, or a
  stand-in. The request goes there and the signed `host` follows it; the
  signature is still scoped to the region.
- **Auth** is `none`. No header carries a key: each request is signed with
  `SigV4` instead, from a credential of kind `bedrock` stored packed as
  `access_key:secret[:session_token]`, the shape the built-in takes. A session
  token is signed, not merely attached.
- **Model ids.** A catalog row's upstream name is Bedrock's model id, or an
  inference profile's (`us.meta.llama3-1-70b-instruct-v1:0`). It goes in the
  path, colon and all, and is signed as it is sent. An ARN works too (an
  application inference profile's, a provisioned throughput's): it stays one
  path segment, its `/` sent as `%2F`.
- **Extra headers** are sent on every request, outside the signature, which
  covers `host`, `x-amz-date`, `x-amz-content-sha256` and the session token.

```sh
oag admin endpoint add --name bedrock-eu --platform aws --dialect bedrock_converse \
  --region eu-west-3
oag admin endpoint add --name claude-tokyo --platform aws --dialect anthropic \
  --region ap-northeast-1
# One key each, filed under the endpoint's name.
oag admin account add --name bedrock-eu-1 --provider bedrock-eu \
  --secret '<access key id>:<secret access key>' --route default
```

Then a catalog row per model, whose provider is the endpoint's name and whose
upstream name is Bedrock's model id: `bedrock-eu/llama-3.1-70b` for
`meta.llama3-1-70b-instruct-v1:0`, priced from Bedrock's price list for that
region.

**What Converse can carry.** Any client dialect reaches it through the
canonical form, and its answer goes back the same way; see
`oag_proto::converse` for the mapping. Three things a client can ask for have no
spelling in Converse and are refused with a 400 rather than dropped: a JSON
answer without a schema (`response_format: json_object`), forbidding tool calls
while tools are defined (`tool_choice: none`), and `previous_response_id`. A
thinking budget and cache breakpoints are dropped, because Converse takes each
only from some model families and a wrong guess is a 400, and so is reasoning
replayed from an earlier turn, which only the model that wrote it can take
back. Tool names are held to the OpenAI pattern, as on Chat Completions, and a
client's own names come back on the calls the model makes.

Converse refuses a conversation it would otherwise take, for a few shapes
clients send all the time, and the codec reshapes each rather than pass the 400
on. A request that declares no tools but whose history called some (a summary,
a compaction) declares a stand-in for each tool called: its name, and a schema
that takes any object. A tool call's id outside Converse's pattern,
`[a-zA-Z0-9_.:-]{1,64}` (Gemini's `read_file#1`, an id past 64 bytes), is
respelled the same way in the call and in its result, and the client's own id
comes back on anything Converse answers with. A conversation that opens with
the model (a prefill, a transcript resumed part way) is sent a `(continued)`
user turn in front of it.

**Streams.** `ConverseStream` sends AWS event-stream messages whose payload is
the event itself, named by a header (see [Framing](#framing)). It announces the
stop before the usage, so the stop is held until the usage arrives, and a client
is shown the bill on the frame that ends the answer. An exception inside the
stream (`throttlingException`, `modelStreamErrorException`, …) reaches the
client as an error frame in its own dialect, naming the exception, and the
ledger records it. So does an event-stream error message, the kind AWS does not
model (`:message-type: error`), named by its `:error-code` and in its
`:error-message`'s words, on Converse and on `InvokeModel` streams alike. A 429
before the stream starts moves to the endpoint's next key, as any provider's
does.

### Azure OpenAI

An endpoint on the `azure` platform is an Azure OpenAI resource, or an Azure AI
Foundry one, speaking the `openai` dialect. Its key is the resource's API key,
sent in `api-key`, the header Azure reads one from, so the endpoint's auth is
`api_key_header`, the only style the platform takes. Microsoft Entra ID is not
served: an Entra token rides as a bearer, and nothing here mints or sends one
yet.

**The base URL is the resource's, and nothing more:**
`https://{resource}.openai.azure.com` or
`https://{resource}.services.ai.azure.com`, where `{resource}` is the
resource's name, 2 to 63 of `a-z`, `0-9` and `-` starting with a letter or a
digit. https, no port, no path: the gateway writes each request's path itself.
Any other host, an address included, is refused for this platform, so a row
cannot point an Azure key, or a request built for Azure, anywhere but at an
Azure resource. A `plain` endpoint may not name `azure.com` at all
([compliance.md](compliance.md#a-plain-endpoint-cannot-reach-a-providers-own-api)).

**Two APIs**, chosen by the row's `api_version`:

| `api_version` | Request |
|---|---|
| unset | Azure's v1 API: `POST {base}/openai/v1/chat/completions`, the deployment named in the body's `model` |
| a version, e.g. `2024-10-21` | the deployments API: `POST {base}/openai/deployments/{deployment}/chat/completions?api-version=2024-10-21` |

Either way a catalog row's upstream name is the **deployment's name**, the name
given to a model when it was deployed to the resource, which is what Azure
routes by in both APIs. In the deployments URL it is one path segment,
percent-encoded (`prod gpt-4o` is sent as `prod%20gpt-4o`); a name that is
empty, `.` or `..` cannot be one, and a request for it fails rather than being
sent. The body is the v1 API's, `model` included: Azure takes the deployment
from the path, and the OpenAI SDK's Azure client sends the same body. The
`api-version` query is built from the row, never from the base URL, which may
not hold a `?`.

**The output ceiling** is sent as `max_completion_tokens`, whatever the
deployment is called, on the v1 API and on every deployments-API version from
`2024-09-01-preview`, the one that added it: a reasoning model refuses
`max_tokens`, every model takes `max_completion_tokens`, and a deployment's
name says nothing of the model behind it. A version from before it has no such
field, and is sent `max_tokens`.

```sh
# Azure's v1 API.
oag admin endpoint add --name azure-eu --dialect openai --platform azure \
  --base-url https://my-resource.openai.azure.com --auth api_key_header
# The deployments API, at a version.
oag admin endpoint add --name azure-eu-dep --dialect openai --platform azure \
  --base-url https://my-resource.openai.azure.com --auth api_key_header \
  --api-version 2024-10-21
# The resource's key, filed under the endpoint's name.
oag admin account add --name azure-eu-1 --provider azure-eu \
  --secret <the resource's API key> --route default
```

Then a catalog row per deployment, whose provider is the endpoint's name and
whose upstream name is the deployment's: `azure-eu/gpt-4o` for a deployment
named `gpt-4o-prod`, priced from Azure's price list.

**Streams** are Chat Completions streams: passed through to an OpenAI-shaped
client byte for byte, Azure's filter results and all, and translated for any
other. The gateway asks every Chat Completions upstream for the stream's usage
(`stream_options.include_usage`), and Azure too wherever it takes the field: on
the v1 API and on every deployments-API version from `2024-09-01-preview`, the
one that added it, and bills what Azure reports. A version from before it
refuses a request that names the field, so a stream through one is sent without
it and reports no usage: **its tokens are not metered**. Name
`2024-09-01-preview` or later, or leave `api_version` unset for the v1 API, to
have Azure streams billed.

**Content filtering.** An answer Azure's filter stops ends with
`finish_reason: "content_filter"`, which reads as a refusal, as OpenAI's does:
an OpenAI-shaped client is told `content_filter`, an Anthropic one
`stop_reason: "refusal"`, a Gemini one `SAFETY`. A prompt the filter rejects is
Azure's own 400, which reaches the client under `error.upstream`, as any
upstream's 400 does, and is not tried on the endpoint's other keys, which would
refuse it too.

**Testing one.** No test can stand up a host under `azure.com`, so a test puts
a mock in a resource's place with `oag_core::endpoint::stand_in_for_azure`,
which admits one loopback origin. It exists only in test builds: it is behind
`oag-core`'s `test-fixtures` feature, which `oag-server` turns on as a
dev-dependency and no release build turns on, so nothing a deployment
configures can widen the rule above. `crates/oag-server/tests/azure_endpoints.rs`
serves both APIs through a running gateway that way.

### Vertex endpoints

An endpoint on the `gcp` platform is Google's Vertex AI in a project and a
region of its own: Gemini through `generateContent`, or Claude through
`rawPredict`. Its credential is a service account's JSON key, and no request
carries the key. The gateway mints a short-lived access token from it and sends
that.

| Dialect | Models | Vertex method, beneath `{host}/v1/projects/{project}/locations/{region}` |
|---|---|---|
| `gemini` | Gemini | `/publishers/google/models/{model}:generateContent`, and `:streamGenerateContent?alt=sse` to stream, in the Gemini API's body |
| `anthropic` | Claude | `/publishers/anthropic/models/{model}:rawPredict`, and `:streamRawPredict` to stream, in Anthropic's body without `model` and with `"anthropic_version": "vertex-2023-10-16"` |

- **Region and project**, both required, are in every request's path. The host
  is the region's own, `https://{region}-aiplatform.googleapis.com`, or
  `https://aiplatform.googleapis.com` for the `global` region.
- **Base URL**, optional, replaces the host: a Private Service Connect
  endpoint, a proxy, or a stand-in. The path beneath it still names the project
  and the region. A multi-region location (`us`, `eu`) has a host of its own,
  `https://aiplatform.{location}.rep.googleapis.com`, so give it as the base
  URL.
- **Auth** is `bearer`: the minted token goes in `Authorization`, and no other
  header carries anything of the credential. There is no `anthropic-version`
  header either; the body carries the version, as Google's own requests do.
- **Model ids.** A catalog row's upstream name is Vertex's model id. Each goes
  in the path as one segment, with everything but letters, digits, `-`, `.`,
  `_` and `~` percent-encoded, so a Claude model's version pin
  (`claude-sonnet-4-5@20250929`) is sent as `claude-sonnet-4-5%4020250929`.
- **Extra headers** are sent on every request, for instance
  `x-goog-user-project` to bill another project's quota.
- `api_version` is not read: requests go to Vertex's `v1`.

```sh
oag admin endpoint add --name vertex-gemini --platform gcp --dialect gemini \
  --region us-central1 --project my-project --auth bearer
oag admin endpoint add --name vertex-claude --platform gcp --dialect anthropic \
  --region global --project my-project --auth bearer
# The service account's JSON key, as Google's console downloads it.
oag admin account add --name vertex-sa-1 --provider vertex-gemini \
  --secret-file ./my-project-sa.json --route default
```

The same two rows as statements (`auth` defaults to `bearer`):

```sql
INSERT INTO endpoint (name, dialect, platform, region, project)
VALUES ('vertex-gemini', 'gemini', 'gcp', 'us-central1', 'my-project'),
       ('vertex-claude', 'anthropic', 'gcp', 'global', 'my-project');
```

`account add` files the key as a credential of kind `service_account`, the one
kind a `gcp` endpoint takes. It reads the key as the gateway's mint will: `type`
is `service_account`, and it has a `client_email`, a `private_key_id` and a
`private_key` that is an RSA key in PKCS#8 PEM, as Google issues them. A key
that could never mint is refused before anything is sealed, and nothing of it is
printed; what is printed is the service account's email. `--secret` and
`OAG_ACCOUNT_SECRET` work too, but a file is what Google hands out. Filed under
two endpoints, one key is two credentials, each with its own token and its own
concurrency.

Then a catalog row per model, whose provider is the endpoint's name and whose
upstream name is Vertex's id for it: `vertex-gemini/gemini-2.5-flash` for
`gemini-2.5-flash`, and `vertex-claude/claude-sonnet-4-5` for
`claude-sonnet-4-5@20250929`, priced from Google's Vertex AI price list.

**What the service account needs.** Permission to call Vertex AI in the project:
`roles/aiplatform.user` is the role commonly granted for it (Google's IAM
documentation currently titles it Agent Platform User). Google's
[Vertex AI access control](https://cloud.google.com/vertex-ai/docs/general/access-control)
page lists what each role holds. A Claude model is served only once it is
enabled for the project in Model Garden, which Google's
[Claude on Vertex AI](https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/claude/use-claude)
page walks through, and which regions serve which model is Google's
[locations](https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations)
page to say.

**The token.** A credential's key signs a JWT (RS256, scope
`https://www.googleapis.com/auth/cloud-platform`, valid for an hour), which the
gateway trades at `gateway.gcp_token_url`, Google's
`https://oauth2.googleapis.com/token` unless it is set, for an access token. The
token is kept until five minutes before it expires, in one cache per gateway
process that every `gcp` endpoint shares and that outlives every reload. So a
credential is minted for at most once at a time, and about once an hour, on
each replica. Each replica mints its own: a mint uses nothing up, unlike an
OAuth refresh, so replicas do not contend. The key's own `token_uri` is never
used, and a redirect from the token endpoint is not followed: a signed
assertion goes to the configured URL or nowhere. A mint goes through the
credential's `proxy_url` when it has one, as its requests do.

The mint happens on the request path, just before the request is built, while
the request holds its slot on that credential; one takes at most ten seconds.

**When a mint fails.** A key Google refuses (`invalid_grant`: the key or its
service account was deleted or disabled, or this host's clock is far enough off
to make the assertion look wrong), a token endpoint that cannot be reached, or
an answer that is not a token, is that credential failing. The request moves to
the endpoint's next credential, as it does when a refresh fails, and the log
names the status and the OAuth error code. A failure is not remembered, so the
next request tries that key again. When no credential can mint, the client is
answered 500 `internal_error`. No log line and no answer holds the key, the
signed assertion or a token.

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
| Bedrock Converse | none: upstream only | no — rendered only as a request to an `aws` endpoint, whose answers are read back into the client's dialect |
| System One | `/jev/v1/systemone` | no — passed through to a System One upstream (Jev, or a System One host), and only to one |

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
cooling down gets the ordinary 503 `no_credential`. The same holds for a
[System One host](#system-one-hosts) a request names: the refusal then names
the host whose key the route lacks.

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

### System One hosts

Jev is not the only server that answers System One. An endpoint registered with
the `system_one` dialect is any host that takes Jev's request and answers in
Jev's shape, and it is served by the System One route beside the built-in Jev:
its keys are leased, retried, failed over and metered exactly as Jev's are, and
its models join System One's catalog, which no chat request can route over.
Merge Gateway's Decisions API is one: the same `state`, `questions` and
`answers`, at `POST /v1/decisions` rather than `/v1/systemone`.

A question set is posted at the endpoint's base URL plus its `path`, which is
`/v1/systemone`, Jev's own, when the row names none. The model listing is read
from the base URL plus `/v1/models` either way. Only a `system_one` endpoint
takes a path, and the column is checked as the rules above say.

```sh
# Merge's Decisions API: System One's shape, at a path of its own.
oag admin endpoint add --name merge-decisions --dialect system_one --platform plain \
  --base-url https://api-gateway.merge.dev --path /v1/decisions --auth bearer
# A Merge Gateway API key, filed under the endpoint's name.
oag admin account add --name merge-1 --provider merge-decisions \
  --secret <Merge Gateway API key> --route default
```

To price its answers, add a catalog row whose provider is `merge-decisions`,
whose upstream name is Merge's model id, `typesafe/jev-1.13`, and whose id is
the two joined: `merge-decisions/typesafe/jev-1.13`. An id may hold several
`/`: the endpoint's name is what comes before the first, and everything after it
is the host's own name for the model.

**Which host answers** is the request's `model` to say, read in this order:

1. none: Jev, which picks its own;
2. an id in System One's catalog: that row's provider, sent the row's upstream
   name — `merge-decisions/typesafe/jev-1.13` reaches Merge as
   `typesafe/jev-1.13`, and `jev/jev-latest` reaches Jev as `jev-latest`;
3. `<endpoint>/<name>` for a System One host this gateway serves: that host,
   sent `<name>`, so a model its listing shows can be asked before anyone
   prices it;
4. a name with no provider in it (`jev-latest`), or Jev's (`jev/…`, or
   `typesafe/…`, its alias and Merge's spelling of Jev's models): Jev, sent the
   body as it arrived — everything this route took before hosts existed;
5. anything else — a chat model, or a host that is not served, one removed or
   one whose row stopped loading — is 400 `no_viable_model`. It is never sent to
   Jev instead: a question set meant for one host is not another's to see.

**The model is the one thing rewritten.** Where a host is sent a name other
than the one the caller used, the body's `model` is replaced and every other
member is copied as the bytes it arrived in, in its order; only the space
between top-level members is not kept. Merge refuses fields it does not know
with 422, which comes back to the caller under `error.upstream`, as any 422
does: the caller's extra fields reach it as they reach Jev. `vendor` and
`customer`, which Merge takes, pass through the same way. Neither Jev nor Merge
streams an answer, and neither does this route.

**Metering.** A host's answer is a ledger row like Jev's, under the model that
answered when a catalog row names it, and otherwise under the row the request
was resolved by: Merge, asked for `typesafe/jev-1.13`, answers as `jev-1.13.0`,
the concrete version, and the row is priced as
`merge-decisions/typesafe/jev-1.13`. The cost is the tokens the host reported
times the catalog's price, as it is for every provider. Merge also reports what
it charged, in `usage.cost`, and that figure is not the ledger's; it reaches the
caller untouched in the answer, which is returned byte for byte, `object`,
`vendor`, `usage.total_tokens` and all.

**The listing.** `GET /jev/v1/models` lists every System One provider the
caller's route holds a key for. A route whose only such keys are Jev's gets
Jev's listing as it arrived, as it always has. Once a host is among them the
gateway writes the listing, in the SDK's shape: Jev's models by their own
names, then each host's, by endpoint name, as `<endpoint>/<name>` — the name
rule 3 sends back to that host. A host's listing is read in either shape a host
is known to use: Jev's `{"models": [...]}`, or Merge's paged
`{"data": [...], "has_more", "next_cursor"}`, whose entries name themselves in
`model`. Merge lists every model it routes, chat included, so an entry that
says what it outputs (in `capabilities.output`, at its top level or under any
vendor) is listed only if a `decision` is among it; an entry that says nothing
is listed. A host is asked for pages of 500, the most Merge allows, and its
cursor is followed until it says there are no more, repeats one, or eight pages
have been read. A host whose listing fails fails the whole listing, as Jev's
always has, rather than quietly leaving its models out.

## Framing

Not every provider streams server-sent events. `ProviderAdapter::framing()`
says which one it speaks, and the default is SSE because all but one do:

| Framing | Providers |
|---|---|
| `Sse` | Anthropic, OpenAI (Chat Completions and Codex), Gemini, Kimi, DeepSeek, Zhipu, xAI |
| `AwsEventStream` | Bedrock, and `aws` endpoints speaking `anthropic` |
| `AwsConverseStream` | `aws` endpoints speaking `bedrock_converse` |

Bedrock streams length-prefixed binary messages whose payload carries the
provider's own event, base64-encoded. A reader that splits on blank lines finds
nothing in one — and the failure is silent: an empty response and zero recorded
usage, with no error anywhere. `eventstream.rs` decodes it.

`ConverseStream` sends the same messages with nothing wrapped: the payload is
the event's JSON as it is, and which event it is is said only by the message's
`:event-type` header (`contentBlockDelta`, `messageStop`, `metadata`, …).
`eventstream::converse_event` puts the name back around the payload, and an
exception message, named by `:exception-type`, becomes an error in Converse's
own terms. Read as `AwsEventStream`, a Converse stream finds no `bytes` envelope
in any message, and says nothing at all.

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
| Bedrock Converse (`aws` endpoints) | Converse's own body and stream, on the Bedrock adapter's host, region and signing. |
| Vertex (`gcp` endpoints) | Gemini's body or Anthropic's at a path naming the project, the region and the publisher; Claude's `model` replaced by `anthropic_version`; a bearer token minted from a service account. |

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
