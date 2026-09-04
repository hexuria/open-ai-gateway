# Every error this gateway returns

For whoever is writing the client. Read from `error_response` in
`crates/oag-server/src/gateway/mod.rs` and the `Error` enum in
`crates/oag-core/src/error.rs` — those two are the source of truth, and
`errors.hurl` pins the ones this deployment can actually produce.

## The envelope

Always this shape, on every failure, in every dialect:

```json
{ "type": "error", "error": { "type": "<kind>", "message": "<human sentence>" } }
```

**Branch on `error.type`, never on the message.** The messages are prose and are
edited; the kinds are a contract. Branch on the status code only as a fallback,
because several kinds share one status and they want different handling — three
different things are 503 and only one of them means "retry shortly".

Two extra fields appear only on `upstream_error`:

```json
{ "type": "error", "error": {
    "type": "upstream_error",
    "message": "…",
    "upstream_status": 400,
    "upstream": { "detail": "The 'gpt-5' model is not supported when using Codex with a ChatGPT account." }
} }
```

`error.upstream` is a **value, not a string** — the provider's own body, parsed.
It used to be stuffed into `error.message`, which meant anything reading
`message` displayed a JSON document to a human. It falls back to a string only
when the body was truncated and no longer parses.

## The catalogue

| `error.type` | HTTP | what it means | client should |
|---|---|---|---|
| `authentication_error` | 401 | no key, unknown key, revoked key | re-auth; do not retry |
| `budget_exhausted` | 402 | a key, route or principal cap is spent | stop; surface which scope from the message |
| `invalid_request` | 400 | the body is not valid JSON for this dialect | fix the request |
| `no_viable_model` | 400 | nothing on the route's ladder can serve it | fix the model, or the operator fixes the ladder |
| `invalid_model_qualifier` | 400 | `@something` is not a qualifier, or that provider cannot be reached that way | drop or correct the qualifier |
| `unsupported_field` | 400 | a field the chosen upstream's dialect cannot express | drop the field, or pin to a provider that has it |
| `not_found` | 404 | an action this gateway does not serve | — |
| `rate_limit_error` | 429 | the route's own rpm limit | **honour `Retry-After`** |
| `no_credential` | 503 | no credential for that provider on this route | retry shortly; operator may need to add one |
| `no_credential_of_kind` | 503 | credentials exist, none of the *kind* pinned | drop the `@` pin, or the operator adds that kind |
| `quota_reserve_held` | 503 | every credential is at its reserve floor | retry after the window resets |
| `at_capacity` | 503 | every credential is at max concurrency | retry shortly — this one really is transient |
| `overloaded` | 503 | this replica is at its in-flight ceiling; the request was shed, not queued | **honour `Retry-After`**; a balancer will land the retry on a replica with room |
| `stream_idle` | 504 | the upstream went quiet mid-stream | retry |
| `upstream_timeout` | 504 | the upstream accepted the connection and never began a response | retry |
| `upstream_error` | *see below* | the provider refused | depends on `upstream_status` |
| `internal_error` | 500 | a bug | retry once, then report |

### `upstream_error` keeps the provider's status, except when it lies

The provider's status is passed through **except 401/402/403/407**, which become
**502**. Those four say the gateway's own credential is bad — expired, revoked,
unfunded, or behind a proxy — and by the time one reaches the client every
credential in the pool has already been tried. Passing a 401 straight through
would tell the caller *their* key was rejected, which is false and sends them to
re-authenticate against the wrong thing.

Not 503 either: 503 means "come back shortly", and a pool of dead keys will not
heal on its own.

So for a client: `upstream_error` + 502 means *the operator has a credential
problem*. `upstream_error` + 400 means *your request*, and `error.upstream` says
why in the provider's own words.

### The three 503s are not interchangeable

`at_capacity` clears on its own in seconds. `quota_reserve_held` clears when a
subscription window resets — possibly days. `no_credential` may never clear
without an operator. A client that treats all 503s as "retry in 1s" will hammer
two of these forever.

### `Retry-After`

Set on **every** 429, both the gateway's own rate limit and a provider's
throttle forwarded through. Rounded up, never zero. Forwarded throttles used to
arrive bare, which meant the 429s a client is most likely to see were the ones
carrying no guidance.

## What `errors.hurl` covers, and what it cannot

Covered, and asserted against a live gateway:

`authentication_error` (missing and malformed), `budget_exhausted`,
`invalid_request`, `no_viable_model`, `no_credential_of_kind`, `not_found`,
`upstream_error` (a model the seat refuses), and the 403 an inference key gets
on the admin listener.

413 is real and verified, but not asserted in the suite: the fixture would be a
five-megabyte blob committed to test one status code. `errors.hurl` carries the
one-line reproduction in a comment instead.

**Not reachable on this deployment**, and why:

| kind | why not | how to see it |
|---|---|---|
| `rate_limit_error` | no route sets `rpm_limit` | set one on a scratch route, or the mock below |
| `at_capacity` | needs every credential saturated | mock, or `max_concurrency: 1` plus concurrent calls |
| `quota_reserve_held` | needs a reserve set and a spent window | `oag admin account set-reserve` on a scratch credential |
| `unsupported_field` | needs an **Anthropic** target; this route serves OpenAI | add an Anthropic credential, then send `response_format` |
| `stream_idle` | needs an upstream that stalls mid-stream | the mock, with a long `MOCK_STREAM_SECONDS` |
| `internal_error` | is a bug | — |

## Simulating the rest without breaking anything

You do not need a provider to actually fail. Three ways, cheapest first.

**1. Make the gateway refuse before it calls anyone.** The best kind of
simulation, because no upstream is involved at all. `budget_exhausted` is the
model case: mint a key with a minuscule quota and it returns 402 on the *first*
request — the cap is already below the hard stop, so nothing is spent and
nothing upstream is touched. `errors.hurl` does exactly this, then revokes the
key. Same trick works for a floor, a reserve, or a disabled credential.

**2. Point a credential at the mock upstream.** `deploy/test/mock-upstream.py`
already exists for the drain and breaker checks, and takes:

```
MOCK_FAIL_STATUS=429   every POST returns this status
MOCK_FAIL_FIRST=2      only the first N fail, then it serves normally
MOCK_STREAM_SECONDS=60 how long a streamed response takes end to end
```

Add a credential whose `provider_base_urls` entry points at it, and you can
produce any `upstream_error` on demand — 429 with its `Retry-After`, a 500, a
502, or a stall that trips `stream_idle`. `MOCK_FAIL_FIRST` is how
`breaker-verify.sh` trips the circuit breaker without a real provider having a
bad afternoon.

**3. Render the shapes without a gateway at all.** `errors.json`, beside this
file, holds every error exactly as the wire carries it — 18 shapes, each with
its status, its `Retry-After` when it has one, and the Rust variant that
produced it. Build a client against those bytes and you need no gateway, no
credential and no failure.

It is GENERATED, by `every_error_shape_matches_the_committed_catalogue` in
`crates/oag-server/src/gateway/mod.rs`, and two mechanisms keep it honest:

- `oag_core::error::every_variant()` is exhaustive over `Error`. It lives in the
  defining crate, where `#[non_exhaustive]` does not apply, and its match has no
  wildcard arm — so adding a variant **fails to compile** until it is listed.
  Verified by adding a variant and watching the build break.
- The test asserts the rendered bytes equal the committed file, so changing a
  status code or a `type` string fails in CI rather than in a client months
  later as an unhandled case.

Regenerate after an intended change, then update the table above:

```sh
UPDATE_ERROR_FIXTURES=1 cargo test -p oag-server error_shape
```

Note the mapping is many-to-one: `UnknownProvider`, `Config` and `Internal` all
render as `internal_error` with the same redacted message, because those can
carry connection strings and file paths. That is why each entry records its
`variant` — otherwise the file shows three identical rows and no reason why.

## Do not branch on the message

Worth repeating, because it is the mistake that survives review. `error.message`
names routes, credentials, reserve percentages and fixing commands, and is
rewritten whenever one of those changes. It is for a person reading a support
ticket. `error.type` is for code.
