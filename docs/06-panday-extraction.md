# What to extract from panday

Panday (`/Volumes/goldcoders/OSS/panday`) was a sibling experiment: a full AI
platform — agent harness, gateway, router, sandbox, plugin system, billing —
spec'd across 21 documents and partially implemented. It is being abandoned in
favour of this gateway. This document records what is worth carrying over, with
enough of the mechanism quoted that the plan survives the repo's deletion.

Surveyed 2026-08-24 against panday `main`. Two housekeeping findings first:

- **`/Volumes/goldcoders/OSS/panday-rotate-grok` is not a separate tool.** It is
  a git worktree of panday parked on `experiment/rotate-grok-oauth`, ~10k lines
  behind `main`. Nothing in it exists only there. Safe to delete.
- **`/Volumes/goldcoders/OSS/panday-m25.2`** is a milestone snapshot. Same.

## The verdict, so nobody relitigates it

As a gateway, this repo is ahead of panday's and the gap is structural, not
polish:

| | open-ai-gateway | panday |
|---|---|---|
| Outbound dialects | 4 (native Gemini, Bedrock event-stream) | 2 (Gemini via Google's OpenAI shim) |
| Inbound dialects | 4 (incl. OpenAI Responses) | 3 (no Responses) |
| Providers | 9 | 4 named + one generic slot |
| Escalation on bad answers | yes, one rung, budget-suppressed | absent — no concept anywhere |
| Counterfactual cost | recorded on every ledger row | `panday_route_counterfactual_usd_total` is declared, registered, rendered, and **never incremented** |
| Streaming under restart | k8s rolling-restart verified in CI | untested |
| DB-path test coverage | runs in CI | 87 of 149 platform tests `#[ignore]`d (need live PG) |

Panday's value is not its gateway. It is a handful of self-contained crates and
files that are genuinely better than what this repo has, plus a set of
hard-won behavioural rules. Those are below, in extraction order.

## Tier 1 — take now

### 1. `panday-sdk/src/oauth.rs` → the Grok connector

One self-contained file (557 lines, deps: serde, serde_json, reqwest, std,
7 inline tests). It is ~90% of the subscription-connector sidecar this repo's
architecture calls for (see compliance.md — subscription bridging stays *out*
of the gateway; a connector presents itself as an OpenAI-compatible upstream
and `gateway.provider_base_urls.xai` points at it).

What it does, so it can be rebuilt if the file is lost:

- Reads `~/.grok/auth.json` (plus colon-separated extras from a
  `*_GROK_AUTH` env var). Every top-level key beginning `https://auth.x.ai::`
  is one xAI OIDC session: `key` (access token), `refresh_token`,
  `expires_at` (RFC3339), `oidc_client_id` (falling back to the suffix of the
  map key). One file can hold several SuperGrok accounts; entries are unioned
  across files and deduped by identical access token.
- Refresh: `GET https://auth.x.ai/.well-known/openid-configuration` → read
  `token_endpoint` → `POST` form-encoded `grant_type=refresh_token` +
  `client_id` + `refresh_token` → new expiry is `now + expires_in − 120s`.
  **`REFRESH_SKEW = 120s`.** Unknown expiry means "try it, refresh on 401" —
  not "assume fresh".
- Refreshes **in memory only, never writes the file back** (a test
  byte-compares `auth.json` before and after). A failed refresh drops the
  token rather than propagating an error.
- The API base is plain `https://api.x.ai` with the OAuth token as a bearer —
  panday's xai adapter is literally its OpenAI-compatible adapter.
- Also reads Claude Code credentials: macOS Keychain
  (`security find-generic-password -s "Claude Code-credentials" -w`), falling
  back to `$CLAUDE_CONFIG_DIR/.credentials.json` then
  `~/.claude/.credentials.json`, parsing `claudeAiOauth.{accessToken,
  refreshToken, expiresAt}`.

Known gaps, inherited honestly: rotated refresh tokens are never persisted, so
a provider that rotates the refresh token on use invalidates the on-disk copy
after one refresh; and **only xAI has a refresh implementation** — the Claude
and Codex readers just drop stale tokens. Panday does not solve this repo's
stubbed Anthropic OAuth refresh.

### 2. `panday-reducer` → the "headroom" token-reduction service

The whole crate: 2,481 lines, 55 tests, deps `panday-types + async-trait +
serde + sha2 + thiserror` (port the handful of IR types it touches). This is
the compression service the dispatch diagram called "Headroom", already built
and already correct about the trap that makes naive compression a net loss.

The five layers:

- **L1 structural**: parse and re-emit terse canonical forms of known tool
  output. Detection is by *content*, not by command name — the harness sees
  `bash` for everything, and `make test` shelling out to cargo should still
  get the cargo treatment. `CargoTest` keys on `test result:` ("the only
  unambiguous marker — 'running N tests' also appears in other runners").
  House rule: a compressor without retention fixtures is rejected.
- **L2 read-dedup**: a re-read returns `Unchanged {seq}` or
  `Changed {seq, diff}`. **The original read stays verbatim in the rolling
  window; only the new result shrinks.** Rewriting the original would churn a
  cached prefix priced at ~0.1× into a fresh one priced at 1× — "a 'saving'
  that costs money."
- **L3 generic**: head/tail line windows with error-looking lines floated into
  the kept region, middle elided behind an expandable marker.
- **L4 spill**: raw output to content-addressed storage (`min_spill_bytes =
  512`); context gets the reduced form plus a handle; exact line ranges are
  retrievable on demand. Reversibility is what makes aggressive defaults safe.
- **L5 semantic**: a cheap-pool model summarises. Deliberately the only layer
  that does IO, at its own async seam.

**The gate is money, not size** (their ADR-007): `value = tokens_removed ×
expected_reads × marginal_price − summarisation_cost − risk`. On a free local
model the value is negative and the gate refuses — there is a test asserting
it. The ADR exists because a benchmarked reducer hit "60–90% reduction and
~0% net cost change, because the channels it compressed were not the ones
being paid for." Dashboards show dollars, never percent.

Unit-of-account bug to not reintroduce: prices are integer micro-dollars **per
million tokens**. An earlier per-*token* unit made $3/Mtok = 0.003 µ$ = `0u64`,
silently pricing every real model as free while the arithmetic kept running.

Placement note: as a separate proxy it must run *behind* routing decisions or
compress only the volatile tail — this gateway's session affinity hashes the
stable prompt prefix, and anything that rewrites that prefix per-turn defeats
the prompt cache it exists to protect. Panday's L2 rule is the same invariant.

## Tier 2 — upgrades to existing crates

### 3. Live ratelimit headroom → `oag-pool`

Panday tracks two independent "remaining" numbers per credential and schedules
on the scarcer:

- **Grant**: operator-declared `{ceiling, window}` counted in **calls** — not
  tokens (unattributable per-credential) and not spend (meaningless on a
  flat-rate seat). Window is per credential: a seat resets in hours, an API
  key in months.
- **Headroom %**: parsed from live response headers —
  `x-ratelimit-{remaining,limit}-{requests,tokens}` (OpenAI/xAI) and
  `anthropic-ratelimit-{requests,tokens}-{remaining,limit}` — reporting the
  scarcer of requests% and tokens%, because "a credential with 90% of its
  requests and 3% of its tokens left has 3% of headroom."

`scarcer(grant, headroom)` is the min of whichever are known; **unknown is
never treated as full** — "a console showing a full bar for a credential
nobody has measured invites exactly the decision the number exists to inform."
An opt-in threshold (default off: "only a 429 proves a credential is actually
spent") skips an entire provider leg *before dialling* when every credential
in the pool is measurably at or below it — it removes rather than reshuffles,
which is kept separate from preference ordering on purpose.

Scheduling rules worth copying with it:

- A **400 does not count against the credential** — "a 400 is the request's
  fault, not the credential's; one malformed caller would open every key in
  the pool." Return immediately, don't advance the failover walk.
- `Retry-After` absent means **abstain** from the soonest-retry min, not zero.
  Parse both delta-seconds and HTTP-date forms.
- Breaker defaults: error **rate** 0.5 over a window of 20 with
  `min_samples = 5` ("without which the first failure on a cold route is a
  100% error rate"), cooldown 10s. Two breaker maps — per `(provider, model)`
  and per credential — over one state machine. Revoking a credential resets
  its breaker so a tripped state isn't left behind for a dead id.
- Breakers and failover judge **establishment only**; a mid-stream failure is
  surfaced, never retried — re-prompting double-bills tokens already seen.

### 4. Wire-protocol hardening → `oag-proto`

Rules panday's translators encode that this hub should be audited against
(several belong to the `api/canonical-fields` line of work):

- **Preserve the provider's original tool-call id.** OpenAI ids are arbitrary
  strings; if the canonical form re-mints ids, keep the original alongside
  (panday: `provider_ids: BTreeMap<u32, String>`) and echo it back as
  `tool_call_id` on the next turn. Losing it is a latent multi-turn tool bug.
- Tool calls stream as fragments keyed by `index`; `id` and `name` arrive only
  on the first fragment. The translator must be stateful.
- Emit terminal `Done` exactly once; some servers send both a `finish_reason`
  and a `[DONE]` sentinel.
- `stop` arrives as a string *or* an array in the wild; accepting one shape
  rejects real clients.
- Anthropic cache-breakpoint indexes past the end of the message list are
  **ignored, not rejected** — "a stale hint should not fail a request that is
  otherwise fine."
- Non-UTF-8 header values are dropped, never lossy-decoded — "a header we
  cannot read is a header we must not guess."
- **Usage is teed as it passes, not recorded at end-of-stream** — "a gateway
  that only records on clean completion under-bills exactly the abandoned
  requests." (This repo already drains on client-gone; the audit is that the
  meter reads the tee, not the terminus.)

### 5. Catalog and router ideas (ideas, not code)

The routing engines differ by design — this repo's tier ladder vs panday's
first-match YAML rules — so nothing transplants wholesale. Worth adopting:

- **`deprecated: true`** keeps a model callable by exact name but removes it
  from auto-selection — "deleting the row would turn pinned requests into
  `ModelNotFound` on the next deploy."
- **Unpriced is not free.** Cost lookups return `Option`; `None` increments an
  `unpriced_calls` counter "rather than a guessed price appearing on a money
  dashboard." Local models carry an explicit zero so "this saved no money" is
  a true statement.
- **Fallback to a pool, not a model** — a frontier rung degrading to the whole
  workhorse pool, not to one hardcoded name.
- **Privacy constraints as an allowlist**, never a denylist — denying
  `frontier` still leaks to `workhorse`.
- **Shadow mode** for classifier changes: run the candidate beside the
  incumbent on the same requests, keep the incumbent's answer, accumulate a
  content-free confusion matrix keyed by stable digest, and gate promotion on
  `ready_for_review(min_compared)`. Panday explicitly rejected A/B routing in
  favour of this; it is the safe road to a learned classifier for `oag/auto`.
  Their classifier constants: trust the heuristic only at confidence ≥ 0.55;
  two conflicting marker families score 0.5 — deliberately below the
  threshold, so ambiguity falls back to the caller's declaration rather than
  being resolved by keyword count. Genuinely ambiguous verbs (`fix`,
  `implement`, `class`) were removed from the marker set entirely after
  substring-match traps ("legit good" contains `git`).

## Shelf — worth keeping, not urgent

- **Credential vault** (`panday-sdk/src/vault/`): XChaCha20-Poly1305 envelope
  encryption with the AAD bound to `id ‖ provider ‖ kind` so ciphertext cannot
  be moved between rows; revoke wipes ciphertext but keeps id/label/last4 so
  history still names the credential. This repo's credential store has no
  encryption at rest today.
- **Entitlement engine** (`panday-platform/src/entitlements.rs`): a pure
  function returning three verdicts — allowed / **degraded** / denied — with
  observed usage passed as an argument, so every test is a pure call. The
  degraded middle is the same philosophy as this repo's budget-pressure
  downgrade.
- **COGS drift reconciliation** (`panday-platform/src/drift.rs`): recorded
  spend vs the provider's invoice, with direction carried separately from
  magnitude (over- and under-recording "are different incidents") and
  models-on-the-invoice-but-not-in-the-ledger as their own class, never
  averaged in. Relevant once real providers are connected.
- **Live model discovery** (`panday-sdk/src/providers/models.rs`, ~200
  free-standing lines): `GET /v1/models` per signed-in provider, handling
  Anthropic's cursor pagination (`has_more`/`last_id`) and both `data` and
  llama-server's `models` response shapes, filtering embeddings/TTS/image
  models. Their stance: the shipped catalog is prices and preferences, "not
  the inventory."
- **Trait-conformance test suites** written before the second implementation
  exists (their cache and vault each ship one) — credited for making the
  Postgres cache implementation cheap. A pattern, not a file.
- **Ledger rebuild-from-log rules**: usage comes from the message event only
  (the turn-summary repeats it — counting both double-bills), and the model
  comes from turn *start* (pricing a session at its first model silently
  disagrees with the live path on exactly the sessions that failed over).

## Leave behind

The agent harness (18k lines, everything hangs off the event-log fold), the
sandbox tiers, billing/Stripe (a projection that "calls nothing"), abuse
controls and public signup, the Leptos console, the GGUF local-model
machinery, and `CredHub` (hardcodes four providers as struct fields). Spec-only
material — hedged requests, the semantic cache, the learn-in-stages ladder —
died as prose; the *rejection reasoning* for semantic caching ("a correctness
hazard for agentic traffic and mostly a demo feature") is worth re-reading
before anyone proposes one here.

Traps recorded so nobody trips them while porting: the dead counterfactual
metric; a `task` hint field that no ingress populates and the router never
reads; sandbox `NetPolicy` fields "that read like controls and control
nothing" (their own run report flags this as a class, not an instance); and a
root `Cargo.toml` with two `exclude` keys in one `[workspace]` table.

## One naming collision, settled

The token-reduction service (item 2) was provisionally called "Headroom",
which collides with panday's **`headroom_pct`** (remaining ratelimit capacity,
item 3) — unrelated concepts sharing a word, both being adopted. Decided
2026-08-24: the reduction service is the **token-reducer**; "headroom" refers
only to remaining ratelimit capacity, which it describes literally. Any
earlier note or diagram that says "Headroom" for the compression service means
the token-reducer.
