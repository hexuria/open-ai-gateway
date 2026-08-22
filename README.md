# open-ai-gateway

An internal AI gateway. One door for every model call your organisation makes,
so you can route the cheap 80% to cheap models, escalate to frontier models only
when the work needs it, rotate across the credentials you own, and see what you
actually spent against what frontier-for-everything would have cost.

Written in Rust. Deployed behind a reverse proxy and a load balancer, in the
three-tier topology that streaming AI traffic actually needs.

> **Not a resale product.** This is built for a single organisation using its
> own credentials for its own members. See [docs/compliance.md](docs/compliance.md)
> for which credential kinds each provider sanctions.

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
just dev      # Postgres + Redis, migrated
just serve    # the gateway on :8080 (inference) and :8081 (admin)
```

```bash
curl -s localhost:8081/health/ready | jq
```

The full three-tier topology — Caddy, Envoy, three replicas — is one command:

```bash
just stack-up
```

## Shape

```
crates/
  oag-core       domain types, errors, typed config       — no I/O
  oag-router     catalog, classifier, tiers, budgets      — no I/O
  oag-proto      wire-format translation hub              — no I/O
  oag-pool       scheduler, session affinity, breakers    — no I/O
  oag-upstream   provider adapters and HTTP transport
  oag-store      Postgres and Redis
  oag-server     axum: two listeners, health, metrics, drain
  oag            the binary
```

Four of the eight crates do no I/O at all. That is deliberate and it is the
main structural bet: routing policy, translation, and credential scheduling are
the things most worth testing exhaustively and least worth spinning up a
database for. The test suite runs in well under a second.

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
| [compliance.md](docs/compliance.md) | Credential kinds and their standing |

## Prior art

Rebuilt from [sub2api](https://github.com/Wei-Shaw/sub2api), which solved the
hard plumbing — credential pooling, prompt-cache-aware session affinity,
streaming usage accounting, two-stage failover — and is worth reading for it.
This keeps the plumbing, drops the resale SaaS around it, and adds the cost
engine that was the point.

## On the name

`open-ai-gateway` reads as "OpenAI gateway" and would be trademark-adjacent for
a published package. That was considered and accepted: this is internal, and
nothing here goes to crates.io or a public registry (`publish = false` across
the workspace). If that ever changes, the name should change with it — the
crates are prefixed `oag-` and the binary is `oag`, so a rename is a
find-and-replace rather than a refactor.

## Licence

MIT.
