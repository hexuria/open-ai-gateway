# open-ai-gateway

An internal AI gateway. One door for every model call your organisation makes,
so you can route the cheap 80% to cheap models, escalate to frontier models only
when the work needs it, rotate across the credentials you own, and see what you
actually spent against what frontier-for-everything would have cost.

Written in Rust. Deployed behind a reverse proxy and a load balancer, in the
three-tier topology that streaming AI traffic actually needs.

> **Not a resale product.** This is built for an organisation to use its own
> credentials for its own members — not to intermediate anyone else's. See
> [docs/compliance.md](docs/compliance.md) for which credential kinds each
> provider sanctions, and why that distinction is one nullable column.

Picking this up on another machine? Start with [HANDOVER.md](HANDOVER.md) — what is
proven, what is not, and where to continue.

## Why

Most requests do not need a frontier model. A routine edit costs roughly
thirtyfold more on `claude-opus` than on `kimi-k2`, and nobody notices the
difference on a routine edit. But asking every developer to pick the right model
every time does not work, so they pick the best one and you pay for it.

The gateway makes that decision centrally, per request, from what it can see
about the task — and records the counterfactual on every row, so the saving is a
number you can point at rather than a claim.

## Quick start

```bash
just dev        # Postgres + Redis, migrated
just bootstrap  # a route, a principal, an API key, the model catalog
just serve      # :8080 inference, :8081 admin + dashboard
```

Then point any Anthropic- or OpenAI-shaped client at it:

```bash
curl -N localhost:29080/v1/messages -H "x-api-key: $OAG_KEY" \
  -d '{"model":"oag/auto","max_tokens":256,"stream":true,
       "messages":[{"role":"user","content":"hello"}]}'
```

Local dev binds **29080** (inference) and **29081** (dashboard) rather than
8080/8081, which collide with nearly every other dev server. Both sit below the
kernel's ephemeral range — macOS hands out 49152+, Linux 32768+ — so they cannot
be claimed out from under you. If they are busy anyway, `just serve` walks up to
the first free pair and prints what it chose; `just ports` shows it in advance.
Containers still listen on 8080/8081 internally, where nothing can collide.

`oag/auto` lets policy choose the model; `oag/cheap` and `oag/frontier` pin a
rung. Name a real model and it is honoured.

Clients that expect the usual discovery and preflight endpoints get them:

```bash
curl localhost:29080/v1/models -H "x-api-key: $OAG_KEY"
```

`/v1/models` lists what *this* key may actually ask for — the route's ladder,
clamped to the key's floor, filtered to providers you hold credentials for —
with the `oag/*` names first. `/v1/messages/count_tokens` returns a prompt-size
estimate without spending anything upstream; it is marked `"oag_estimate": true`
because no tokeniser is linked.

The full three-tier topology — Caddy, Envoy, three replicas, Postgres, Redis —
is one command, and Prometheus and Grafana are one flag more:

```bash
just stack-up
```

The dashboard is on the admin listener at `http://127.0.0.1:29081/`; Grafana, if
you brought up the observability profile, at `:3000`.

## Shape

```
crates/
  oag-core       domain types, errors, typed config       — no I/O
  oag-router     catalog, classifier, tiers, budgets      — no I/O
  oag-proto      wire-format translation hub              — no I/O
  oag-pool       scheduler, session affinity, breakers    — no I/O
  oag-upstream   provider adapters and HTTP transport
  oag-store      Postgres and Redis
  oag-server     axum: two listeners, health, metrics, drain, admin API
  oag            the binary
web/index.html   the dashboard, embedded in the binary
```

Four of the eight crates do no I/O at all. That is deliberate and it is the
main structural bet: routing policy, translation, and credential scheduling are
the things most worth testing exhaustively and least worth spinning up a
database for. The suite is ~180 tests and runs in well under a second.

The dashboard is a single self-contained HTML file compiled into the binary. A
build toolchain and `node_modules` for a handful of read views is what "less is
more" was defined against, and an operator debugging a gateway at 3am should not
need `npm install` first. The admin API is ordinary REST, so a richer UI can be
added later without the server changing.

## Two listeners

- **`:8080`** — inference only. What the load balancer fronts, and the only port
  that needs to be reachable.
- **`:8081`** — admin API, SPA, `/metrics`, `/health/ready`. Internal network.

Serving both from one port means every admin endpoint inherits the inference
endpoint's exposure. Splitting them makes "do not expose the admin API" a
deployment fact rather than a routing rule someone has to remember.

## Documentation

| | |
|---|---|
| [00-architecture.md](docs/00-architecture.md) | The tiers and the request path |
| [01-deployment.md](docs/01-deployment.md) | Topology, **the seven things that break streaming**, scaling |
| [02-cost-routing.md](docs/02-cost-routing.md) | Tiers, classification, escalation, budgets |
| [03-providers.md](docs/03-providers.md) | The adapter contract |
| [04-cloud.md](docs/04-cloud.md) | **Cloud deployment** — Kubernetes, Cloud Run, Fargate, Container Apps, Cloudflare |
| [05-services.md](docs/05-services.md) | Capability-service catalog — register, health-check, deep-link |
| [compliance.md](docs/compliance.md) | Credential kinds and their standing |

## What it does

- **Routes by cost.** A route defines an ordered tier ladder. Requests are
  classified from what is observable — prompt size, tool count, conversation
  depth, whether extended thinking was asked for — and served from the cheapest
  rung that can do the job.
- **Escalates when the cheap answer is unusable.** Refusals, truncation,
  malformed tool calls, and empty responses trip a quality gate and retry one
  rung up. Bounded to one rung, and *suppressed* when the principal is near
  their budget: escalating then would undo the saving the downgrade made.
- **Pools credentials.** Priority tiers, least-loaded selection, use-it-or-lose-it
  window preference, LRU, circuit breakers, and two-stage failover.
- **Keeps prompt caches hitting.** Conversations pin to a credential, keyed on
  the part of the prompt that is stable across turns. On agentic traffic the
  cache is most of the bill.
- **Speaks both dialects, in both directions.** Anthropic Messages and OpenAI
  Chat Completions, inbound and upstream, translated through one canonical form.
- **Measures itself.** Every request records what it cost *and* what it would
  have cost on the route's top tier. The difference is the point.

## Prior art

Rebuilt from [sub2api](https://github.com/Wei-Shaw/sub2api), which solved the
hard plumbing — credential pooling, prompt-cache-aware session affinity,
streaming usage accounting, two-stage failover — and is worth reading for it.
This keeps the plumbing, drops the resale SaaS around it, and adds the cost
engine that was the point.

## On the name

`open-ai-gateway` reads as "OpenAI gateway" and is trademark-adjacent. The
repository is public; the crates are not published to crates.io (`publish =
false` across the workspace), which is where a name collision would actually
bite. Worth reconsidering before that changes — the crates are prefixed `oag-`
and the binary is `oag`, so a rename is a find-and-replace rather than a
refactor.

This project is not affiliated with, endorsed by, or connected to OpenAI.

## Licence

MIT.
