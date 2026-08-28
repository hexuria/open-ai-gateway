# Capability gaps: what a government deployment needs that we do not have

**Status: research. Nothing here is committed work.** Written 2026-08-27 after
comparing OAG against Aperture by Tailscale. It exists to be argued with, and to
be picked up later without re-deriving the reasoning.

---

## 1. The shape of the answer

OAG's job is to be the **door**: authenticate the caller, decide which model and
which credential, translate dialects, record what it cost. Everything below is
either a check applied *at that door* or a service the door *calls out to*.

That distinction is not a style preference. It is already in the schema:

```sql
kind text NOT NULL CHECK (kind IN
  ('sandbox', 'tool', 'guard', 'reduce', 'harness', 'browser', 'other'))
```

`guard` is a service kind we designed for and never wired. The catalog registers
a service, health-checks it, and deep-links to its own UI — and **the request
path never calls one**. So most of what follows is not "build a guardrail
engine". It is "build the hook that lets a registered guard see a request, and
then choose a guard".

Two consequences worth stating plainly:

- **Do not absorb these into the gateway.** A PII classifier is a model. A
  sandbox is a container runtime. Embedding either makes the gateway something
  it is bad at being, and couples its release cycle to theirs. The existing
  `service` row is the right shape.
- **Some things cannot be a service.** Identity and rate limiting must be at the
  door, because their whole purpose is to reject a request before it costs
  anything. A guard that runs after the model has answered is an audit tool, not
  a control.

---

## 2. What the government context changes

This is the largest input and it reorders everything.

| Concern | Consequence |
|---|---|
| **Accountability** | "Who asked this, and what did the model see" must be answerable months later, by someone who was not there. Our ledger records *tokens and cost*, never content. That is privacy-preserving and audit-hostile at the same time. |
| **Data egress** | Every prompt leaving the building is a disclosure. A guard that inspects and blocks *before* the upstream call is the control; logging after the fact is not. |
| **Identity** | A bearer token in a config file is not an identity. It cannot be revoked per-person on offboarding, cannot be tied to a directory, and cannot answer "was this person authorised on that date". |
| **Residency / air-gap** | Any capability that is a hosted SaaS is likely unusable. This rules out most managed guardrail vendors and pushes hard toward self-hosted, open-source components. |
| **Determinism of controls** | "The model was asked not to" is not a control. Controls must be mechanical: a regex/classifier that blocks, a quota that refuses, a role that denies. |

**Implication:** the ranking below is *not* the ranking I would give a startup.
For a startup, cost routing is everything and guardrails are a checkbox. Here,
guardrails and identity are the reason the system is allowed to exist at all.

---

## 3. The capabilities

### 3.1 Inbound rate limiting

**What it is, plainly.** A cap on *how often* a caller may make a request,
independent of money. "This key may make 60 requests per minute and 5,000 per
day." It is not the same as a budget: a budget stops you when you have spent
$100, which could be one enormous request or ten thousand small ones. A rate
limit stops the ten thousand.

**Answering the specific question:** yes — this is limiting an API key, and yes,
a "5-hour window" limit is exactly this shape. Restricting *which models* a key
may reach is a related but distinct control (authorisation, not rate), and we
already have a partial form of it in the route ladder and `--floor-tier`.

**Why it matters here.** A runaway agent is the normal failure, not the
exceptional one. Today a valid key can issue unlimited concurrent requests and
the only brakes are a dollar budget (which a flat-rate seat never trips, since
its marginal cost is zero) and the new quota reserve (which only engages once the
provider's own pool is nearly spent). A looping agent on a subscription seat can
therefore exhaust a shared weekly pool in minutes and nothing refuses it.

**What we have.** Verified: nothing. Every `rate_limit` path in the codebase is
*upstream* 429 handling — `select.rs`, `schedule.rs`, `usage_poll.rs`. There is
no inbound limiter.

**Low level.**

- Where: a `tower` layer on the public router, before routing and before
  credential selection, so a refused request costs one Redis round trip.
- State: Redis, which is already a hard dependency and already shared across
  replicas. A token bucket or a fixed window per `(api_key_id, window)`. Per-key
  is the minimum; per-principal and per-route are the natural extensions.
- Limits live on the key or the principal as typed columns, next to
  `monthly_budget_usd` — not in a generic settings table (see `config.rs` for why
  this repo refuses those).
- Response: HTTP 429 with `Retry-After`, and the body should name which limit was
  hit. A limiter that says only "too many requests" produces a support ticket.
- **The trap:** streaming. A limiter that counts *requests* under-counts a
  workload that opens one stream and pulls a million tokens through it. Consider
  a concurrency cap (in-flight streams per key) alongside the rate — we already
  track in-flight counts for credential scheduling, so the machinery is familiar.
- **Second trap:** a distributed limiter that reads-then-writes races across
  replicas. Use an atomic Redis operation (`INCR` with expiry, or a Lua script),
  not read-modify-write.

**Effort:** small. This is the cheapest item here and the one whose absence is
most likely to cause an incident.

---

### 3.2 OIDC / SSO

**What it is, plainly.** OIDC (OpenID Connect) is the standard by which an
application asks an identity provider — your organisation's directory, e.g.
Entra ID, Keycloak, Okta — "who is this person, and are they still employed
here?" SSO is the user-facing result: people sign in once with their work
account instead of holding a per-application secret.

Concretely, it replaces *"here is a long-lived key, don't lose it"* with *"prove
you are you, right now, and I will issue you something short-lived."*

**Why it matters here.** Three things a bearer token cannot do:

1. **Offboarding.** Someone leaves; their directory account is disabled; their
   gateway access ends the same moment. Today you would have to know which keys
   were theirs and revoke each one.
2. **Attribution that survives audit.** `principal.email` is a string we typed.
   An OIDC subject is a claim signed by the directory — evidence rather than
   assertion.
3. **Group-derived authorisation.** "Members of the Analysts group may reach the
   frontier tier" becomes expressible without maintaining a second copy of the
   org chart.

**What we have.** `principal.oidc_subject` exists as a column and **nothing in
the codebase reads it**. It is a stub someone left for this exact purpose. (The
only OIDC references in code are xAI's `oidc_client_id`, which is upstream OAuth
and unrelated.)

**Low level.**

- Two distinct surfaces, and conflating them is the usual mistake:
  - **The dashboard / admin API** — interactive. Authorization Code + PKCE, a
    session cookie, group claims mapped to `principal.role`. This is the
    straightforward half.
  - **The inference path** — non-interactive. A coding agent cannot do a browser
    redirect. Options: (a) keep API keys but *mint* them through an SSO'd flow so
    every key has a verified owner and an expiry; (b) accept the IdP's own JWT as
    a bearer token and validate it against JWKS; (c) device-code flow for CLIs.
    **(a) is the pragmatic first step** — it changes provenance without changing
    every client.
- Key lifetime becomes meaningful once keys have verified owners: add expiry and
  last-used, and a `key list` that shows both.
- Candidates: the `openidconnect` crate for the relying-party side; Keycloak,
  Authentik or Dex as a self-hostable IdP if the department has none. All
  self-hostable, which the residency constraint likely requires.
- **Not a replacement for keys on day one.** Aperture's trick — no keys anywhere,
  identity from the network — depends on every device being on their mesh. We
  cannot copy that, and should not pretend to.

**Effort:** medium for the dashboard, medium-large for the inference path.

---

### 3.3 Guardrails — the one that matters most here

**What it is, plainly.** A check that inspects a request *before* it reaches the
model, and the response *before* it reaches the user, and can block or modify
either. Typical checks: personal data (names, national ID numbers, addresses),
secrets (keys, credentials), classification markings, prompt-injection patterns,
and which tools an agent may invoke.

**Why it matters here.** For a government deployment this is the control that
makes the system defensible. Everything else is efficiency; this is the reason
you are allowed to send anything to a third-party model at all. It is also the
one that must be **mechanical** — a system prompt asking the model to behave is
not a control, because the model is the thing you are constraining.

**What we have.** Nothing in the request path. But `kind = 'guard'` already
exists in the service catalog, so the vocabulary and the registration/health-check
half are done.

**Low level.**

- **The hook is the work.** A `guard` service registered in the catalog gets
  called at two points: pre-upstream with the canonical request, and
  post-upstream with the response (streaming makes the second hard — see below).
  Contract: the guard returns allow / block-with-reason / transformed-content.
- **Fail-closed vs fail-open must be a per-guard setting, and default closed.**
  If the PII guard is down and the gateway keeps serving, the guard is
  decorative. That decision belongs to the operator, in config, stated loudly.
- **Streaming is the hard part, and it is unavoidable here.** You cannot inspect
  a complete response you have not finished streaming. Options: buffer the whole
  response for guarded routes (kills time-to-first-token, may be acceptable for
  the traffic that needs guarding); inspect incrementally with a sliding window
  and cut the stream mid-flight (complex, and a partial leak has already left);
  or restrict guarded routes to non-streaming. **Decide this explicitly**; it is
  a product decision, not an implementation detail.
- **Latency budget.** Every guard is on the critical path. A local classifier at
  ~10ms is fine; a remote LLM-as-judge at 800ms doubles your p50. Prefer
  deterministic checks (regex, dictionary, NER) for the mandatory path and keep
  model-based judging for sampled/async review.
- **Candidates to evaluate** (all self-hostable, all need verification before
  adoption — I have not run any of these):
  - **Microsoft Presidio** — PII detection and redaction, purpose-built, the most
    obviously aligned with a government requirement.
  - **LLM Guard** (Protect AI) — a suite of input/output scanners.
  - **NeMo Guardrails** (NVIDIA) — programmable rails; heavier, more general.
  - **Guardrails AI** — validator framework.
  A deterministic in-house regex/dictionary pass for locally-defined markings and
  ID formats will likely be needed regardless, because those are jurisdiction
  specific and no off-the-shelf tool knows them.
- **The audit tension.** Blocking requires inspecting content; inspecting content
  creates a record you must then protect. Decide what a *blocked* request stores:
  the full prompt (useful, sensitive), a hash and the rule that fired (defensible,
  less useful), or a redacted excerpt. Aperture's answer is configurable retention
  down to zero, which is a reasonable model to copy.

**Effort:** the hook is medium. Choosing, deploying and *tuning* a guard is the
larger and less predictable half — false positives are what kill these rollouts.

---

### 3.4 MCP

**What it is.** Model Context Protocol — a standard by which a model reaches
tools and data sources. In gateway terms there are two separable jobs:

1. **Proxying/aggregating MCP servers**: agents point at the gateway, the gateway
   fans out to registered MCP servers and applies access control — which server,
   which tool, which caller. This is the governance play, and it is the same
   shape as `kind = 'tool'` in our catalog.
2. **Exposing our own MCP endpoint**: letting an agent query the gateway itself
   (spend, models, routes) as a tool.

**Why it might matter here.** If agents in this deployment use tools at all, MCP
traffic bypasses the gateway entirely today — you would be governing model calls
while tool calls, which touch real systems, go unwatched. That is a strange place
to stop for a government system.

**What we have.** Zero MCP anywhere in the codebase. The `tool` service kind is
the nearest hook.

**Effort:** large, and the most speculative item here — *if we build it*. We
should not. See below.

#### Open Connector (openconnector.dev) — the tool-side twin

Verified from the site. Positioning, in its own words: *"an open-source,
self-hostable AI integration platform"* — *"Integrate once to connect AI agents
to hundreds of services over MCP or API, while keeping credentials and a
tamper-evident audit trail in your control."* TypeScript, **AGPL-3.0**,
`github.com/openconnector-dev/openconnector`, self-hostable with a managed beta.
~30 connectors (Slack, GitHub, Gmail, Jira, Drive, Stripe, Salesforce, …).

**The important observation: it is the same architecture as OAG, applied to the
other half of an agent's world.**

| | OAG | Open Connector |
|---|---|---|
| Holds credentials for | model providers | services and tools |
| Governs | what an agent **says** | what an agent **does** |
| Routes | model calls | tool/API calls |
| Records | tokens, cost, counterfactual | actor, scope, result |

That is a clean division, not an overlap. It also answers §3.4 directly:
**do not build MCP proxying into the gateway.** Register Open Connector as a
`kind = 'tool'` service, let it own tool credentials and tool audit, and keep OAG
owning model credentials and cost. Each stays the thing it is good at, and the
existing catalog row is the integration.

Three things to weigh before committing:

- **AGPL-3.0 is a live question for a government deployment, not a footnote.**
  The AGPL's network clause obliges you to offer source to users of a modified
  version reachable over a network. Running it unmodified internally is the easy
  case; forking it — which is exactly what a jurisdiction-specific connector set
  would require — is the case that needs legal sign-off. **Raise this early.** It
  is the kind of constraint that surfaces late and invalidates months of work.
- **Its audit design is better than ours and worth borrowing regardless.**
  *"Every action writes a hash-chained record — actor, scope, timestamp,
  result."* A hash chain makes the log tamper-evident: altering an entry breaks
  every link after it. Our audit is plain `tracing` lines with no such property.
  For a government system that difference is the difference between a log and
  evidence. Cheap to adopt in our own audit trail whether or not we use their
  product.
- **It cannot be self-hosted today, because the source is not published.**
  Checked 2026-08-27: `github.com/openconnector-dev/openconnector` has 2 stars,
  0 forks, **2 commits**, and a README reading *"The project source code is being
  prepared for public release and will be published here soon."* No install
  guide, no compose file, no image, no stated stack. The pricing page's promise —
  *"Self-host the whole core under AGPL-3.0 — forever"*, unlimited connections,
  including the MCP gateway, token vault and audit trail — is a real and generous
  commitment, but it is a commitment about a future artefact. **There is nothing
  to integrate with yet.**

- **The features this deployment specifically needs are in the unfinished paid
  tier.** Their Enterprise tier is *"under active development; checkout is
  paused"* and is where **SSO (SAML & OIDC), SCIM directory sync, and long-term
  audit retention** live. For a government system those are not add-ons, they are
  entry requirements. So the free self-hosted core would cover tool credentials
  and connectors while leaving identity and retention to be solved elsewhere —
  which is the same place we already are.

**Revised verdict: the design is right, this product is not yet a dependency.**
Keep tool governance as a separate service behind a `kind = 'tool'` row — that
decision holds regardless of vendor. Watch Open Connector; do not sequence work
against it. Re-evaluate when the source actually lands and the Enterprise
identity story is real. A system that must be accredited cannot take a hard
dependency on a two-commit repository.

**Open question:** does it cover the tools this deployment actually needs, or are
those bespoke internal systems? Thirty SaaS connectors is a strong answer for
Slack and Jira and no answer at all for a departmental mainframe.

---

### 3.5 Log export / SIEM

**What it is.** Shipping the audit and usage record into the organisation's
existing security monitoring, rather than expecting anyone to log into our
dashboard.

**Why it matters here.** In government the SIEM is usually mandated, and "we have
our own dashboard" is not an accepted answer. Aperture exports to S3 for exactly
this reason.

**What we have.** A ledger in Postgres, an audit line on admin writes
(`target: "oag::audit"`), Prometheus metrics. No export.

**Low level.** Structured JSON to stdout is already the shape (`log_json`), so
the cheapest path is a collector — Vector or Fluent Bit — rather than code in the
gateway. A periodic ledger export (S3/object store, partitioned by day) covers
the usage half. Note the config already documents that `otlp_endpoint` is
**declared but not implemented** and rejected at startup; wiring real OTLP is a
separate, honest piece of work.

**Effort:** small to medium, and mostly deployment rather than code.

---

### 3.6 Teams / projects

**What it is.** A grouping above the individual, so cost and policy attach to a
department or a project rather than to one person.

**Why it matters here.** Chargeback across departments is usually the funding
model. Today `principal` is flat: a person, a budget, a role. There is no
expression of "the Analytics team's spend" other than summing people by hand.

**Low level.** A `team` table, `principal.team_id`, budgets and quotas resolvable
at either level with the tighter one winning, and reporting grouped by it. The
reporting layer is already period-aware after the recent work, so this is mostly
a join and a group-by. Aperture's "Projects" is the same idea.

**Effort:** medium. Worth doing *before* the ledger grows large, because
backfilling attribution later is unpleasant.

---

### 3.7 Tool-call tracking

**What it is.** Recording which tools an agent invoked, not just how many tokens
it spent. Aperture breaks this out and categorises it.

**Why it matters here.** Tool calls are where a model stops talking and starts
*acting*. For a government audit trail, "the model wrote a file" is a more
important record than "the model used 4,000 tokens".

**What we have.** `oag-proto` already parses tool-use blocks in every dialect —
`ToolUseStart`, `ToolUseDelta`, `ToolUseEnd` are canonical stream events. The
data passes through us and we discard it.

**Low level.** Count and name tool calls per request in the accumulator (cheap,
already in hand), and store either a count or a `usage_event_tool` child table.
Storing tool *arguments* is a content-retention decision with the same tension as
guardrails — decide it once, in the same place.

**Effort:** small. The parsing is done; this is plumbing an existing signal into
the ledger.

---

### 3.8 Provisioning employees into third-party services

A problem raised by the operator, and one the connector literature mostly skips:
an organisation with many employees needs each of them to have an account on
several third-party services. Nobody wants each employee doing that themselves,
and the organisation — although it owns and administers the mail domain — does
**not** want to read employees' mail. But service signup usually demands a
verifiable email address.

**The reframing that solves it: the problem is the verification *channel*, not
the mailbox.** Do not arrange to read employee mail carefully. Arrange for
verification mail never to arrive there.

**The principle, and why it matters more than the mechanism.** Do not rely on a
*policy* of "we will not read their mail". Make it structural: the automation
holds credentials only for mailboxes that contain no human correspondence. Then
"we do not read employee mail" is a property of the system rather than a promise
a reviewer has to accept. It also removes the single most alarming scope from the
security review — a Gmail *read* scope under domain-wide delegation, which
otherwise dominates that conversation (see §3.2 and the delegated-vs-application note below).

**Three tiers, chosen by what the target service supports.**

1. **SSO / SCIM — no verification email exists.** Where a service supports
   SAML/OIDC with SCIM provisioning, nobody signs up at all: identities come from
   the directory. There is no verification mail to intercept because there is no
   signup. **Always check for this first** — it removes the problem rather than
   working around it, and many services offer it on business tiers.
2. **Domain verification — verify once, for everyone.** Many services let an
   admin prove control of `company.gov` via a DNS TXT record, after which any
   address on the domain joins without individual verification.
3. **A real reachable address — route it away from the person.** Provision using
   a dedicated subdomain whose mail lands in an automation mailbox the platform
   team owns:

   ```
   alice.gmail@svc.company.gov  ->  automation mailbox
   bob.jira@svc.company.gov     ->  automation mailbox
   ```

   The employee keeps their own identity *inside* the service; only the
   verification channel is centralised. The automation reads `svc.company.gov`,
   which contains nothing but machine mail, and never touches `alice@company.gov`.

**The operational payoff is larger than the privacy one.** Under tier 3 the
organisation — not the employee — owns the account's recovery path. When someone
leaves, the service account and its password-reset route do not leave with them,
which is the usual way organisations lose control of third-party accounts. It
also makes the mailbox fully auditable, because there is nothing personal in it
to protect.

**Gotchas to design for:**

- **Verification links are bearer capabilities.** Whoever holds the link is the
  user. That mailbox is a secret store: restricted access, short retention,
  audited reads. This is the one place to be strict rather than pragmatic.
- **Subaddressing (`alice+jira@`) is rejected by some services**, and
  disposable-looking domains by others. A real subdomain with proper MX records
  behaves like an ordinary domain and avoids both.
- **Phone or authenticator-based MFA is not solved by mail routing.** Expect a
  minority of services to need a human, and budget for it rather than discovering
  it late.
- **Do not collapse into shared logins.** One account used by many people
  destroys attribution, which is the thing a government system can least afford
  to lose — and it is the tempting shortcut when provisioning gets tedious.

**Where OAG fits: mostly it does not, and that is correct.** Registration is a
one-time provisioning concern that belongs in an onboarding process, not in the
request path. What OAG owns is the *credential lifecycle afterwards* — the
resulting tokens sealed at rest under the KEK, refreshed with version guarding,
bound to a principal. That machinery exists. The provisioning flow should hand
tokens to it and then get out of the way.

## 4. What we deliberately should not build

- **Response caching.** Neither we nor Aperture have it. It is genuinely useful
  for cost, and genuinely dangerous in a multi-tenant government context: a cache
  keyed carelessly serves one user's answer to another. If it is ever built, the
  key must include the principal, and it should be off by default.
- **Our own PII classifier.** That is a model, maintained by people who do that
  full time. Call one.
- **An identity provider.** Integrate with the department's; do not become one.
- **Aperture's network-identity trick.** It works because Tailscale owns the
  network. We do not, and imitating it without the mesh gives the appearance of
  identity without the substance.

---

## 5. Suggested order, and why

1. **Inbound rate limiting** — smallest, and its absence is the most likely cause
   of a live incident (a looping agent draining a shared seat).
2. **Tool-call tracking** — small, the signal already flows through us, and it
   materially improves the audit story.
3. **Guardrails hook + one deterministic guard** — the item that makes the system
   defensible. Start with the hook and a locally-defined regex/dictionary pass;
   add a real classifier once the hook is proven.
4. **OIDC on the dashboard**, then key provenance on the inference path.
5. **Teams** — before the ledger grows, not after.
6. **SIEM export** — largely deployment.
7. **Service provisioning (§3.8)** — not gateway work, but it gates every tool
   integration and the mail-boundary decision should be made before anyone
   registers the first account. Cheap to decide now, expensive to unpick later.
8. **Tool governance** — adopt Open Connector as a `tool` service rather than
   building MCP into the gateway. Sequence it whenever agents in this deployment
   start touching real systems, which for a government project is likely sooner
   than the model-side polish.

Borrow **hash-chained audit records** from Open Connector's design into our own
audit trail early and independently of any adoption decision. It is a small
change that turns a log into evidence, and retrofitting it after a year of
records is worth less than having it from the start.

## 6. Open questions for the operator

- Has AGPL-3.0 been cleared for this deployment? It gates Open Connector, and the
  answer likely arrives from lawyers rather than engineers.
- Do the tools this project needs exist as Open Connector connectors, or are they
  bespoke internal systems that would need writing either way?
- Which target services support SSO/SCIM (tier 1) or domain verification
  (tier 2)? That census decides how much of §3.8 tier 3 has to exist at all.
- Is there an existing departmental IdP to integrate with, or would we be
  standing one up?
- Is the deployment air-gapped, or egress-restricted-but-connected? This decides
  whether hosted guardrail services are even candidates.
- Which jurisdiction's PII and classification rules apply? That determines how
  much of the deterministic guard must be written locally rather than adopted.
- For guarded routes: is buffering acceptable (losing time-to-first-token), or
  must streaming be preserved? This is the single biggest design fork in §3.3.
