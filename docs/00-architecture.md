# Architecture

## In ten sentences

Every model call an organisation makes goes through one door. At that door the
gateway authenticates the caller, checks their budget, decides which model
should serve the request, picks a credential from the pool, translates the
request into whatever dialect that provider speaks, streams the response back
while translating it in the other direction, and records what it cost alongside
what it would have cost on the best model available. If the credential fails, it
picks another. If the model produces something unusable, it retries one tier up.
Postgres holds the truth; Redis holds what the replicas need to agree on. The
gateway itself is stateless, so you run as many as you need behind a load
balancer.

## Crates

Four of the eight do no I/O at all. That is the main structural decision.

| Crate | Responsibility | I/O |
|---|---|---|
| `oag-core` | Domain types, errors, typed config | none |
| `oag-router` | Catalog, classification, tier ladders, escalation, budgets | none |
| `oag-proto` | Wire-format translation hub | none |
| `oag-pool` | Scheduler, session affinity, circuit breakers | none |
| `oag-upstream` | Provider adapters, HTTP transport | network |
| `oag-store` | Postgres, Redis | database |
| `oag-server` | axum: two listeners, health, metrics, drain | network |
| `oag` | The binary | — |

Routing policy, translation, and credential scheduling are the things most worth
testing exhaustively and least worth spinning up a database for. Keeping them
pure means the interesting tests run in milliseconds and the CI job that runs
them needs no services at all.

## The request path

```
POST /v1/messages
  │
  ├─ tower       request id · header-read timeout
  │              (never a whole-body timeout)
  │
  ├─ auth        Bearer / x-api-key / x-goog-api-key → sha256
  │              from the headers, before the body is read
  │              → moka L1 (short TTL + negative cache)
  │              → Redis L2 (HMAC with signing_secret; single-flight on miss)
  │              → Postgres
  │              invalidated by Redis pub/sub; then budget and route checks
  │
  ├─ body        DefaultBodyLimit after a key has resolved
  │
  ├─ ROUTE       passthrough? honour the named model, subject to floor tier
  │              managed?     classify → pick tier → pick model
  │                           budget pressure → downgrade a tier
  │                           nothing capable enough → escalate a tier
  │
  ├─ pool        sticky lookup: hash the cache-marked prompt prefix
  │                             → Redis → pinned credential?
  │              else cascade:  eligible → min priority → least loaded
  │                             → soonest window reset → LRU
  │              acquire a Redis slot; check the breaker
  │
  ├─ translate   client dialect → canonical → upstream dialect
  │
  ├─ forward     per-(credential, proxy) client from a bounded pool
  │              refresh the credential if it is close to expiry
  │              bounded retry with backoff
  │
  ├─ stream      reader task → bounded channel → SSE writer
  │              upstream idle watchdog · downstream keepalive
  │              client gone → keep draining upstream, the tokens are billed
  │              a mid-flight death is named; the buffer is capped
  │
  ├─ meter       merge usage patches → price actual and counterfactual
  │              → usage_event per attempt; every attempt lands and is charged
  │              (PK is (request_id, attempt) since 0014)
  │
  └─ on failure  classify → retry same credential
                          → or escalate a tier (context reject included)
                          → or cool down, exclude, and re-enter the pool
```

## Why the hub

Four dialects are in play — Anthropic Messages, OpenAI Chat Completions, OpenAI
Responses, Gemini `generateContent` — and any client dialect may need to reach
any upstream one. Pairwise converters would be twelve, each with its own
streaming state machine. Everything converts to and from one canonical
representation instead, so a fifth dialect costs two converters rather than
eight.

The canonical shape is Anthropic Messages, because it is the most expressive of
the four: explicit content blocks, tool results as first-class content, and
cache breakpoints. Lowering from it loses less than raising to it would gain.

## Failure model

Two independent mechanisms, and keeping them separate is what stops a provider's
bad afternoon from quietly migrating the fleet onto expensive models:

- **Failover** handles *credential* problems — auth failures, rate limits,
  overload, stalled streams. Cool the credential down, exclude it, pick another.
- **Escalation** handles *capability* problems — refusals, truncation, malformed
  tool calls, context overflow. Retry one tier up, once.

`Error::disposition` in `oag-core` is the single pure function that decides
which, so the whole policy is unit-testable without a network.

## What is in Redis, and why losing it is survivable

Concurrency slots, session pins, and the auth cache (HMAC'd with
`signing_secret`, so a replica holding a different one ignores the others'
entries). All three are expendable: losing Redis costs a burst of database
reads and a moment of sloppy concurrency accounting. It never loses money or
credentials, because those are in Postgres.
