# Running locally

Six ways to run this on your own machine, a first-run sequence that ends in a
request, and the one command to run when a request fails.

## The run modes

They differ in what they prove, and they cost accordingly. Start at the top; go
further down only when you need the thing that mode adds.

| Command | What runs | What it costs to start | Use it for |
|---|---|---|---|
| `just dev-serve` | Postgres + Redis in Docker, migrated, then the gateway as a **host binary** (one replica) | A debug `cargo` build | Everyday development. Fast rebuilds — the gateway is not containerised. |
| `just serve` | The gateway host binary only; assumes `just dev` already brought the infrastructure up | An incremental rebuild | Restarting the gateway without touching the database. |
| `just stack-up` | The **full topology** in Docker: Caddy → Envoy → three replicas → Postgres → Redis | A release image build, so a few minutes | Rehearsing the production shape: TLS termination, the proxy hop, and more than one replica sharing Redis. |
| `just floci-up` | The `gcp-cloudrun` OpenTofu stack applied against a **local GCP emulator**, which really starts the gateway as a Cloud Run service | Docker, `terraform`/`tofu`, and a pull of `ghcr.io/hexuria/open-ai-gateway:main` | Rehearsing the cloud deploy — that the stack applies, and that what it starts serves. See `deploy/floci`. |
| `just floci-cloudsql` | The same, with the database as a **real Cloud SQL instance** floci provisions | As above | The managed data tier, which is what a real GCP deploy uses. |
| `just verify` | The whole request path against a mock upstream | About a minute, **no credentials** | Checking the gateway works at all, on a machine with no provider account. |

`just floci-up` pulls a published image rather than building yours; set
`OAG_IMAGE` to point it at a local build.

Ports: the gateway binds **:29080** (inference) and **:29081** (admin API and
dashboard) locally — not 8080/8081, which collide with roughly every other dev
server. If either is taken, `just serve` walks upward to the first free *pair*
and prints what it chose, so a clash shifts the port instead of failing the run.
Override the starting point with `just pub_port=31000 adm_port=31001 serve`.
Inside containers the listeners are still 8080/8081, where nothing can collide.

Three sibling verifications need no credentials either, and each pins one thing:
`just verify-breakers` (two 408s trip the breaker; the third request is refused
without another upstream call), `just verify-dialects` (the OpenAI and Gemini
adapters against `aimock`; needs Node), and `just verify-translate` (an OpenAI
client over an Anthropic mock — the translation hub rather than a native
adapter).

## First run, end to end

One shell, from a cold checkout. The exports are the same values the `just`
recipes use, so commands you run here and recipes you run later see the same
database; they are dev-only and hardcoded on purpose.

```bash
just dev                # Postgres + Redis, migrated. Leave the containers up.

export OAG_DATABASE__URL=postgres://oag:oag@127.0.0.1:5452/oag
export OAG_REDIS__URL=redis://127.0.0.1:6399
export OAG_SECURITY__SIGNING_SECRET=dev-only-signing-secret-do-not-use-in-production-0001
export OAG_SECURITY__CREDENTIAL_KEK=b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=
oag() { cargo run --quiet -p oag -- "$@"; }
```

Then, in order — each step exists because the next one fails without it:

```bash
oag admin init --email dev@localhost      # principal + route + the first ADMIN key
oag admin catalog seed                    # prices and context windows

# One upstream. --secret is read from OAG_ACCOUNT_SECRET if you omit it, which
# keeps the key out of shell history and out of the process table.
oag admin account add --name anthropic-1 --provider anthropic --secret sk-ant-...

# The ladder the router climbs, cheapest rung first.
oag admin route tiers --route default \
  cheap=anthropic/claude-haiku-4.5 \
  balanced=anthropic/claude-sonnet-4.5 \
  frontier=anthropic/claude-opus-5

oag admin key create --email dev@localhost --name codex   # the INFERENCE key
oag admin doctor                                          # should print `ok`
```

`just serve` in another shell, and the gateway is up.

Two things that first sequence leaves at their defaults:

- **The route is in `passthrough` mode.** A client naming a concrete model gets
  that model; only `oag/*` names are routed by policy. `oag admin route mode
  managed --route default` applies policy to every request — and then the model
  a client asks for is *ignored*, which is the single most confusing thing about
  a managed route. `docs/02-cost-routing.md` covers the trade.
- **The credential is an API key.** A subscription seat is imported instead of
  typed: `oag admin account add --name grok-seat --from grok` (or `--from
  codex`), which reads the CLI's `auth.json` and never writes it. Which
  providers offer which is in `docs/03-providers.md`; `oag admin providers`
  prints the same matrix from the running build.

Shortcuts for the impatient: `just bootstrap` runs `admin init` plus `catalog
seed` and prints the key; `just catalog-update` seeds from LiteLLM's live table
instead of the built-in snapshot (needs network, ~2MB).

## The two keys — and why they differ

Every key is minted against a principal and a route. Authority to reach the
**admin** API is a property of the **key**, not the principal (`api_key.admin`):
an ordinary inference key is deliberately refused on `/admin/api`, because that
key gets pasted into SDK configs and CI, and leaking it must not also hand over
the admin surface. So there are two kinds:

| | Inference key | Admin key |
|---|---|---|
| Mint | `oag admin key create --email dev@localhost` (`just key`) | the same, plus `--admin` (`just admin-key`) |
| Sends requests (`/v1/messages` on :29080) | yes | yes |
| Reaches the dashboard and `/admin/api` (:29081) | no — 403 | yes |
| Goes in | an SDK or client config | the dashboard key box only |

The key `oag admin init` prints is an **admin** key. Pasting an inference key
into the dashboard returns 403 and looks exactly like a broken dashboard, so
mint the two separately and label them (`just key name=codex`,
`just admin-key name=ops`). A key is shown once — only its SHA-256 is stored.

## Using them

```bash
# Inference — point any Anthropic client at the gateway:
curl -N localhost:29080/v1/messages -H "x-api-key: $INFERENCE_KEY" \
  -d '{"model":"oag/auto","max_tokens":256,"stream":true,
       "messages":[{"role":"user","content":"hello"}]}'

# What this key may actually reach: the route's ladder, intersected with the
# live credentials that can serve it right now (empty if this key is exhausted).
curl localhost:29080/v1/models -H "x-api-key: $INFERENCE_KEY"
```

OpenAI clients hit `/v1/chat/completions`, Responses clients `/v1/responses`,
Gemini clients `/v1beta/models`; all four dialects reach the same router.

Dashboard: open <http://127.0.0.1:29081/>, paste the **admin** key into the key
box (top right), and click **Load**. It belongs only in that box — not the
browser address bar, where it lands in history and in any proxy log.

## When a request fails

```bash
oag admin doctor            # --route <name> for a route other than `default`
```

It checks, in the order a request meets them: migrations applied, catalog
non-empty, the route exists and its ladder parses, credentials attached to the
route and their live state (`ready` / `disabled` / `cooling down` / `rate
limited`), whether every rung has a provider that can actually serve it, and
whether Codex has the `gateway.codex.instructions` it refuses to work without.
It prints the fixing command beside each failure and exits non-zero, so it works
in a script as well as by eye.

The traps it exists to catch, because none of them look like their cause:

- **An empty catalog fails every request**, with a routing error rather than
  anything mentioning the catalog.
- **A rung naming a provider you hold no credential for** is a hole the router
  falls through only when traffic reaches that tier — so it passes at cheap and
  fails at frontier.
- **A Codex seat with no configured instructions** is refused by the backend and
  reads as a dead credential.

`oag admin status` (routes, credentials, this month's spend) and
`oag admin account list` answer "what is registered"; `doctor` answers "why
would this fail".
