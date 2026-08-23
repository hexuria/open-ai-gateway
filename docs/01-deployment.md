# Deployment

## The three tiers

The usual framing is `reverse proxy → load balancer → API gateway → service`.
For AI traffic that is one box too many.

Those are three *roles*, not necessarily three processes:

| Role | Question it answers | What changes for AI traffic |
|---|---|---|
| **Reverse proxy / edge** | Terminate the connection safely | TLS, HTTP/2, and an allow-list of the paths it will proxy at all. Must not compress or buffer `text/event-stream`. |
| **Load balancer** | Which replica? | **Least-request, never round-robin.** Completions vary 100× in duration. Needs outlier ejection and very long draining. |
| **API gateway** | Is this caller allowed, and where does this request really go? | Authn, budget, model routing, credential selection, translation, metering. |

**A generic API gateway cannot be the API-gateway tier here.** Kong, Apigee, and
AWS API Gateway all assume request cost is known upfront and responses are
short. AI breaks both:

- Cost is unknown until the stream finishes, so quota is reconciled afterwards.
- Routing is model- and credential-aware, not path-aware.
- The gateway holds a pool of *upstream* credentials and picks one per request.
- AWS API Gateway hard-caps at 29 seconds. Most managed gateways buffer SSE.

So `open-ai-gateway` **is** the API gateway tier, in the sense that LiteLLM,
Portkey, and Envoy AI Gateway are. Putting Kong in front of it buys a hop, a
buffering hazard, and duplicated auth.

```
            org clients (Claude Code, Codex CLI, SDKs, CI jobs)
                            │  HTTPS, internal network
                            ▼
  ┌──────────────────────────────────────────────────────────┐
  │  EDGE — Caddy                                            │
  │  TLS · HTTP/2 · inference paths ONLY, no catch-all        │
  │  compression allow-list that EXCLUDES text/event-stream   │
  └──────────────────────────────────────────────────────────┘
                            │  h2c, no buffering
                            ▼
  ┌──────────────────────────────────────────────────────────┐
  │  L7 LOAD BALANCER — Envoy                                │
  │  LEAST_REQUEST · outlier ejection · no stream timeouts    │
  │  health check /health/ready · long drain                  │
  └──────────────────────────────────────────────────────────┘
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
  ┌──────────────────────────────────────────────────────────┐
  │  AI GATEWAY × N (stateless)                              │
  │  :8080 inference        :8081 admin + /metrics            │
  └──────────────────────────────────────────────────────────┘
              │                              │
              ▼                              ▼
      Postgres (truth)              Redis (coordination)
```

Being internal deletes a whole category: no WAF, no DDoS scrubbing, no captcha,
no IP reputation, no public rate limiting.

**Caddy and Envoy collapse into one** for a small deployment — Caddy does both
tiers. Envoy earns its place the moment you run more than one replica, for three
things Caddy does not do well: least-request balancing, outlier ejection, and
draining that respects long-lived streams.

## The seven things that break streaming

Almost every generic setup gets at least two of these wrong. They all produce
the same symptom — the client sees nothing, then everything at once, or a 504 —
and none of them look like a proxy problem.

### 1. Buffering

Off at every hop.

- **Caddy** buffers by default but detects `text/event-stream` and flushes
  immediately, so leave `flush_interval` unset. Setting it to `-1` forces
  immediate flushing for *every* response, losing the buffering that ordinary
  JSON benefits from.
- **Envoy** streams by default. Just do not add a buffer filter.
- **nginx**: `proxy_buffering off; proxy_request_buffering off;`

### 2. Compression

**Never compress `text/event-stream`.** A compressed SSE stream is buffered
until the stream ends, so the client sees nothing for two minutes and then
everything at once — indistinguishable from a hung backend.

The trap is that `text/event-stream` matches `text/*`, and `proxy_buffering off`
does **not** disable nginx's gzip filter. Use an allow-list of concrete content
types, never a wildcard. See `deploy/caddy/Caddyfile`.

### 3. Timeouts

Idle and read timeouts must exceed the longest stream you will serve. The
defaults are all far too short:

| Where | Default | Set to |
|---|---|---|
| Envoy `route.timeout` | **15s** | `0s` |
| Envoy `stream_idle_timeout` | **5m** | `0s` |
| nginx `proxy_read_timeout` | 60s | `1800s` |
| Caddy transport `read_timeout` | — | `0` |

Envoy's 15-second route timeout is the single most common cause of "my LLM proxy
works locally and 504s in production."

The gateway itself sets a header-read timeout and a connection idle timeout, and
deliberately **no whole-response write timeout** — any deadline on the complete
response severs a legitimately long completion. Stalls are caught by the
gateway's own idle watchdog, which can tell "thinking" from "gone".

### 4. Header underscores

nginx silently drops headers containing `_`, which breaks session-affinity
routing that keys on one. Needs `underscores_in_headers on;`. This gateway
sidesteps it by using `-` in all its own headers.

### 5. Load balancing policy

`LEAST_REQUEST`, not `ROUND_ROBIN`.

Completions vary by two orders of magnitude in duration. Round-robin distributes
*arrivals* evenly, which distributes *load* very unevenly: one replica ends up
holding every long stream while its neighbours idle. sub2api's bundled Caddyfile
ships `round_robin`.

### 6. Draining

A rolling deploy must not sever in-flight streams. The sequence:

1. `SIGTERM` arrives.
2. `/health/ready` starts failing **immediately**, so the load balancer ejects
   this replica within a few health-check intervals.
3. In-flight streams keep going, up to `gateway.max_stream_duration`.
4. Exit when they finish or the budget expires.

Step 2 is what makes it work, and only if your orchestrator's grace period
exceeds the drain budget — `stop_grace_period` in compose,
`terminationGracePeriodSeconds` on Kubernetes.

sub2api gives in-flight work a hardcoded five seconds, so every deploy drops
every active stream.

### 7. Health versus readiness

- `/health/live` — the process is running. **Never checks dependencies.** A
  liveness probe that fails during a database outage makes the orchestrator
  restart every replica, turning a recoverable incident into a crash loop.
- `/health/ready` — Postgres and Redis are actually reachable, and we are not
  draining. **This is what the load balancer checks.**

sub2api's `/health` returns a static `{"status":"ok"}` regardless of database
state, so a replica with a dead pool stays in rotation and spreads the failure
to every client instead of being routed around.

## Running more than one replica

The gateway is stateless. Everything shared is in Postgres or Redis. Two
requirements:

**Signing secrets must be supplied externally and be identical everywhere.**
The gateway refuses to boot without `security.signing_secret` and
`security.credential_kek`, and refuses anything that looks like a placeholder.
sub2api generates its equivalents per instance when the environment does not
supply them, so replica A mints tokens replica B rejects.

`signing_secret` also authenticates the shared auth cache in Redis, so a replica
holding a different one still authenticates correctly — it just ignores the
others' cache entries and reads Postgres instead. Rotating the secret is
therefore safe and needs no flush: every existing entry stops verifying, and the
in-process caches age out within 15s. There is no dual-secret window, so plan on
one cold cache after a rotation.

**Concurrency slots expire by TTL and nothing else.** sub2api runs a cleanup at
every boot that removes every Redis slot not carrying the current process's
randomly-regenerated prefix — which, with more than one replica, removes every
slot held by every *other* live replica. Any restart or scale-up silently voids
concurrency accounting fleet-wide. A replica that dies here leaves its slots
behind for at most one TTL: bounded, and self-healing.

Migrations are safe to run from every replica simultaneously; a Postgres
advisory lock serialises them.

## The admin API

It performs writes: disable or enable a credential, clear a cooldown, revoke a
key. Four operations, chosen because they are what you reach for during an
incident. Everything you can do calmly at a prompt stays in `oag admin`.

**Authority is on the key, not the principal.** `api_key.admin` is what the
admin API checks. This matters because `oag admin init` prints a key the
operator is meant to paste into an SDK config, and its principal is an admin —
so under a principal-based check, every inference key that operator ever minted
could disable credentials. Mint the admin one explicitly:

```bash
oag admin key --email you@example.com --route default --name ops --admin
```

Every `/admin/api` route is authenticated by a single layer applied where the
routes are declared, not by a call inside each handler. A handler that forgets
the call is silently public and looks exactly like the others; a route in the
wrong function is visible in ten lines. `/`, `/metrics` and `/health/ready` sit
outside it on purpose — the page must load before anyone can type a key, and
the scraper and the orchestrator do not have one.

So **reachability is the only control over those three**, which makes it the
edge's problem, and an edge does that job as an allow-list rather than a
catch-all. `deploy/caddy/Caddyfile` proxies the inference paths and answers 404
to everything else; the admin site is a separate listener that
`deploy/compose/stack.yml` does not publish. The Helm ingress does the same
thing by routing the `public` Service port and omitting the admin one. A
`handle { reverse_proxy oag-1:8081 }` fallback — which is what the bundled
Caddyfile used to end with — puts the dashboard and every gauge in `/metrics` on
the public TLS vhost, and `/admin/api` one key behind it.

Auth is an `x-api-key` header rather than a cookie, so a cross-origin page
cannot forge a write: it would need to set a custom header, which requires a
preflight the browser refuses. The dashboard keeps the key in `localStorage`,
which is a real exposure and is why the page ships a restrictive CSP — that
narrows the channel, it does not close it, since nothing in CSP constrains
top-level navigation.

**With `single_listener: true`** — which Cloud Run and Container Apps force —
the admin routes are merged onto the public listener and there is no second
listener to keep off it. `disable` and `revoke` are then reachable with one key;
the dashboard, `/metrics` and `/health/ready` are reachable with none, because
they were never behind the key in the first place. Restrict the service with
ingress rules or IAM. The key is not covering this.

Every write emits one line at `warn` on the `oag::audit` target, naming the
actor, the action and the subject. `warn` rather than `info` because the log
filter is a free-form string and an operator quietening a noisy deployment to
`warn` would otherwise erase the audit trail without noticing.

## Migrations

`oag migrate` runs them under a Postgres advisory lock; `oag serve` does not run
them at all. There is one migration, and it is a baseline that gets edited in
place rather than accumulating a chain — which is only safe while no database
anyone cares about has applied it.

sqlx checksums applied migrations. Against a database that ran an earlier
version of `0001`, `oag migrate` fails closed with *migration 1 was previously
applied but has been modified* and applies nothing — including any later
migration. In development the fix is to recreate the database. In production it
is a hand-patched `_sqlx_migrations.checksum`, so once this project has a real
deployment the baseline stops being editable and changes become `0002`, `0003`,
and so on.

## Verifying it

```bash
just stack-up      # Caddy -> Envoy -> 3 replicas -> Postgres + Redis
just stack-roll    # restart one replica under load
```

The test worth writing first drives a three-minute stream through the whole
stack and asserts chunk timing **at the client**, not at the origin. It catches
every one of traps 1, 2, and 3 at any hop, which is why it is a test rather than
a paragraph in this file.
