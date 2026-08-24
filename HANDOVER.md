# Handover

State of `open-ai-gateway` as of 2026-08-24, written to be picked up on another
machine with no memory of the session that produced it.

Everything below was verified the way it says it was. Where something was *not*
verified, it says so — those are the entries that matter most.

## What this is

An internal AI gateway whose point is cost: route the boring majority of
requests to cheap models, escalate to frontier only when the work warrants it,
rotate across whatever credentials the organisation owns, and record what was
actually spent beside what frontier-for-everything would have cost. Not a resale
product; no payment code, deliberately.

## Where it stands

Green: 390 tests (`cargo test --workspace -- --list`), clippy clean at
`-D warnings`, fmt clean. The four no-I/O crates (`oag-core`, `oag-router`,
`oag-pool`, `oag-proto`) carry 214 of those. `ci` and `k8s` are green on `main` at `5d5ff4f` (the #18 merge) — `k8s` 8 of
8 streams, 8 ledger rows, as on every stack merge that ran it today. The
release workflow publishes `ghcr.io/hexuria/open-ai-gateway:main` for
linux/amd64 and linux/arm64; it is publicly pullable.

`main` is clear. There is no open PR. Every 2026-08-23/24 stack is merged:

| Stack | PRs | What landed |
|---|---|---|
| catalog | #19 | `services` table, admin Services panel, `docs/05-services.md` |
| edge | #14 | Caddy no longer publishes admin on the catch-all |
| admit | #10 #11 #13 | Gemini panic (C2), auth before the body (S2), Redis auth-cache HMAC (S1) |
| money | #1 #2 | dialect-aware collect + meter (C1/H2); charge only the written ledger row |
| avail | #5 #6 #8 #9 | breaker probe, escalate on context reject, RAII lease, upstream 401 not the client's |
| api | #15 #16 #18 | SSE death + buffer cap, proto renderer names, `tool_choice` / `response_format` / `stop` carried or refused |

Leftover remotes with those stack names still exist. They are already in `main`.
Do not reopen them.

These are **shipped**. They are not pickup. The first live credential is still
the first pickup; nothing below invents a live-provider story.

```bash
just verify           # Anthropic request path vs the Python mock, ~1 min, no credentials
just verify-breakers  # circuit breaker trips; the next request never hits upstream
just verify-dialects  # OpenAI + Gemini adapters vs aimock; needs Node (`npx`)
just verify-bedrock   # Bedrock event-stream vs VidaiMock; needs Docker
just verify-translate # OpenAI client → Anthropic mock; the hub, not a native adapter
just verify-k8s       # rolling restart severs no stream; needs kind, ~10 min
just check            # fmt + clippy + tests
just dev-serve        # local gateway; also what the editor run button launches
```

`just verify-k8s` also runs in CI (`.github/workflows/k8s.yml`) on every push
touching `crates/`, `deploy/` or `migrations/`, on PRs, and nightly. As of
2026-08-23 it passes: 8 of 8 streams complete across a full `rollout restart`
and all 8 reach the ledger. That claim used to be an anecdote from one manual
check; it is now checked by a runner. It still only proves drain — not
breakers. See pickup #3.

`just verify` is the one to run first on a new machine. It needs no Node and no
keys. It passes today with:

```
anthropic/claude-haiku-4.5 on 'cheap': in=100 out=18,
$0.00019000 vs $0.00285000 frontier — 93% saved, ttft 998ms
```

That figure is against the mock. Nothing has talked to a real provider.

## What landed — keep as shipped

### Catalog — #19

`migrations/0002_services.sql` is the capability-service catalog. The admin
dashboard has a Services panel. `docs/05-services.md` is the slice: register a
row, health-check it, deep-link to the service's own UI. The gateway does not
become the sandbox. Do not delete that doc.

**0002 is the catalog.** It is not the money migration.

### Edge — #14

Caddy proxies the inference paths and 404s everything else. The admin listener
is not on the catch-all. Documented in `docs/01-deployment.md`. Cloud Run and
Container Apps still force `single_listener: true`; that caveat is unchanged.

### Admit — #10 / #11 / #13

- **C2 / #10**: a non-object Gemini body no longer panics the replica.
  `catch_panic` is insurance; the request path answers first.
- **S2 / #11**: inference `route_layer` authenticates from headers before a
  handler's `Bytes` extractor runs. An anonymous POST is 401, not a 256 MiB
  allocation. Default `max_body_bytes` is 32 MiB.
- **S1 / #13**: the Redis auth cache is HMAC'd with `signing_secret`. A
  replica with a different secret ignores the others' entries and reads
  Postgres. Rotation needs no flush.

### Money — #1 / #2

Dialect-aware collect on the non-streamed path, and the abandoned escalation
attempt is metered instead of thrown away (C1/H2). The key is charged only
when the ledger row is actually written — a replay or a dropped second
attempt does not debit.

`migrations/0003_usage_event_attempt.sql` is the expand: column `attempt`,
unique index on `(request_id, attempt)`. The primary key on `request_id`
**survives this release**, so a rolling deploy does not 42P10. A later
release contracts by dropping that PK. Until then a second row for one
request is still dropped. That is expand/contract, not a bug to reopen.

Do not write that money changes become `0002`. `0002` is already the catalog.

### Avail — #5 / #6 / #8 / #9

Breaker half-open probe is spent on admit and released if the request is not
sent. A `ContextOverflow` climbs a rung, including on a streamed request that
was refused before any bytes. A credential's Redis slot is returned on drop.
An upstream 401 is not mapped to "the client's key is wrong".

Unit-tested. Not e2e in kind — see pickup.

### API — #15 / #16 / #18

A stream that dies mid-flight says so; the SSE buffer is capped. Renderers
keep the model name and the tool name. `tool_choice`, `response_format`,
`stop`, and `previous_response_id` are carried where the dialect has a place
for them, and refused with a 400 naming the field and the dialect where they
do not.

### Verify-mocks

`just verify-breakers`, `just verify-dialects`, `just verify-bedrock` and
`just verify-translate` run in CI as the `verify-mocks` job
(`.github/workflows/ci.yml`). Dummy secrets only.

What those add, verified 2026-08-24:

- **Breakers.** Two inbound 408s (three same-account retries each) trip the
  threshold of 5. A third request returns `503 no_credential` and the mock's
  POST count stays at 6. 5xx/529 cannot prove this: they cool the account via
  the scheduler before the breaker ever sees five failures.
- **OpenAI and Gemini adapters.** Non-stream and stream, both dialects, through
  the gateway against [aimock](https://github.com/CopilotKit/aimock) 1.39.0.
  Ledger rows have non-zero tokens and a counterfactual above actual. Dummy
  keys (`sk-mock`). Not a real provider.
- **Bedrock event-stream.** `just verify-bedrock` points the adapter at
  VidaiMock with `deploy/test/vidaimock/` overlay. Frames are AWS event-stream
  with CRC32 and `{"bytes": "<base64 of Anthropic event>"}` — an encoder this
  repo did not write. Non-stream invoke and a live stream both meter 100/12
  and beat the frontier counterfactual. Dummy IAM pair; SigV4 is signed but
  not verified. This is not AWS's wire.
- **Cross-dialect translation.** `just verify-translate` sends Chat Completions
  at `oag/auto` with only an Anthropic mock account on the route. The client
  gets `chat.completion` / `chat.completion.chunk` + `[DONE]`; the mock sees
  `POST /v1/messages`. A passthrough would have leaked Anthropic SSE. Ledger
  rows are the cheap Anthropic model: 100/12 non-stream, 100/`CHUNKS*3` stream.

## Pick up here

In priority order. Local and CI verification of the stacks above is done.
Everything here is still blocked on credentials, a real cloud account, or a
decision.

### 1. Point it at one real credential

Nothing has ever talked to a real provider. Anthropic, OpenAI and Gemini have
now been E2E'd against mocks of their wire formats. That is not the same as
AWS's Bedrock framing, Anthropic's OAuth token endpoint, or Anthropic's
tokenizer.

```bash
just dev-serve
oag admin add-account --name anthropic-1 --provider anthropic --secret sk-...
```

Then send a request through `oag/auto` and read the ledger row. This also
unblocks three things that cannot move without it:

- **Bedrock vs AWS**: `just verify-bedrock` proves the decoder against
  VidaiMock's encoder (correct `{"bytes":}` wrap, real CRC32). It does not
  prove AWS's framing. A real Bedrock call is still the only way to catch a
  vendor-specific header or CRC quirk. aimock's Bedrock path is still the
  wrong inner payload — do not point `provider_base_urls.bedrock` at it.
  Overlay and survey: `deploy/test/vidaimock/`, [docs/05-llm-mocks.md](docs/05-llm-mocks.md).
- **OAuth**: the two-layer refresh (process mutex, fleet lock, version stamp,
  `invalid_grant` recovery) is the most concurrency-sensitive code in the repo
  and has only unit coverage. `AnthropicAdapter::refresh` is still the default
  `Ok(None)` — the HTTP call to Anthropic's token URL does not exist yet. A mock
  of that endpoint is a small custom thing, not aimock.
- **`count_tokens` calibration**: the divisors in `oag_proto::count_input_tokens`
  are reasoned, not measured. Run five real prompts through Anthropic's own
  `count_tokens` and adjust. Until then the `oag_estimate: true` flag is doing
  real work. llm-mock exposes `/v1/messages/count_tokens` but documents it as
  an approximation (probed: `"Hello!"` → 2). Not a tokenizer.

### 2. Run `deploy/tofu/verify-migration-gate.sh` once per cloud

This script has never been run against a real cloud.

Migrations run on all three clouds, by three different mechanisms, because the
providers expose three different things — `docs/04-cloud.md` has the table and
the reasoning. Whether a *failed* migration actually fails the apply is argued
on GCP and AWS and never observed.

The script applies cleanly, corrupts the migration ledger the way a broken
migration would, forces a redeploy, and asserts the second apply fails. Use
`data_mode = "neutral"`: cheaper, seconds to provision, and it leaves the
database reachable, which is what makes the corruption step possible.

The two unknowns that would change the design if they came out badly:

- **GCP**: that the provider surfaces a FAILED Cloud Run execution as an apply
  error rather than a ready-but-failed resource. The whole GCP guarantee rests
  on this one behaviour.
- **Azure**: whether `revision_mode = "Single"` holds traffic on the previous
  revision until the new one is *healthy*, or shifts on *provisioning*. If the
  latter, a failed migration on upgrade is a total outage on a green apply —
  strictly worse than the bug the migrate step was added to fix.

Azure cannot fail the apply at all; azurerm exposes no revision health and no
revision data source. It is fail-*closed* — the gateway never starts in a replica
whose migration failed — and the stack outputs `migrate_check` with the command
to run after each apply. Documented, not hidden.

### 3. Smaller, all self-contained

- **Circuit breakers in kind**: `just verify-breakers` now proves the trip end
  to end against the local mock — two 408s, and the third request is refused
  without touching upstream. What it does not prove is the same behaviour
  inside kind under a rollout: `deploy/test/kind-verify.sh` still only proves
  drain — no `MOCK_FAIL_*`, no breaker assertion. The mock has
  `MOCK_FAIL_STATUS` and `MOCK_FAIL_FIRST` for exactly this, and
  `kind-verify.sh` is a working template for that kind of test. Do not treat
  #5 as having closed this.
- **In-cluster Postgres** is a single StatefulSet with no operator, no PITR and
  no pooling. Fine for kind; wrong for the credential store. Use CloudNativePG
  with `data.mode=external`.
- **`/v1/models` in passthrough** returns the ladder plus every off-ladder
  catalog model, rendered per request. The built-in catalog is small; the
  documented seeding path is LiteLLM's table, which is >1000 entries. Memoise
  against the catalog `Arc` if that bites — do not cap the list, which would
  make the answer wrong rather than large.
- **`route_providers` alias asymmetry**: still true as of 2026-08-24.
  `Provider::from_str` accepts `moonshot` / `glm` / `grok`. `oag admin
  add-account` parses and stores `provider.as_str()`, so the CLI path is
  fine. `select::lease` → `candidates` queries `a.provider = $2` with the
  canonical spelling. A row written as `moonshot` (SQL, or any path that
  skips the parse) is advertised by `/v1/models` and never actually
  selectable. Normalise on write or filter the listing.
- **CRC32 on our event-stream test encoder.** The tests write prelude/message
  CRC as `0` (`oag-upstream/src/eventstream.rs`). VidaiMock now checks the
  decoder against real CRCs end to end; filling in the unit-test encoder is
  still a self-contained change, no aimock, no Node.

## Things that will bite if you do not know them

- **aimock is a dev/CI dialect stand-in, not an upstream.** Do not point a real
  deployment at it. Do not replace `deploy/test/mock-upstream.py` in `just verify`
  or `just verify-k8s`: that mock is stdlib-only so it can run from a ConfigMap
  with no image. aimock needs Node.
- **The Cloud Run stack's plan is never clean.** The API does not return
  `run_execution_token`, so it re-diffs every plan. That is what makes the
  migration execute. Do not silence it with `ignore_changes`, and do not use
  `terraform plan -detailed-exitcode` as a drift gate there.
- **Migrations must be expand/contract.** In all three clouds the migration lands
  while the previous release is still serving — on AWS for up to the 1800s
  deregistration delay.
- **The migration chain is no longer "changes become 0002".** There are three
  files:
  - `migrations/0001_baseline.sql` — still edited in place, and that is only
    safe while no database anyone cares about has applied it.
  - `migrations/0002_services.sql` — the capability catalog. Shipped.
  - `migrations/0003_usage_event_attempt.sql` — the ledger expand. Shipped.
    PK on `request_id` still present; contract is a later release.
  sqlx checksums applied migrations and `oag migrate` will fail closed,
  permanently, against a database that ran an earlier version of a file.
  The moment there is a real deployment, stop editing `0001`. Changes become
  `0004` from then on.
- **Admin authority is on the key, not the principal** (`api_key.admin`). Mint one
  with `oag admin key --admin`. An ordinary inference key from an admin's own
  principal is deliberately refused — it gets pasted into SDK configs.
- **Local dev binds 29080/29081**, not 8080/8081, which collide with everything.
  `just serve` walks up to the first free pair and prints what it chose.
- **Three GitHub identities exist on the machine this was built on**, and only
  one owns this repo. `hexuria` owns it; the `hexuria.github.com` remote is an
  SSH alias in `~/.ssh/config` that selects `~/.ssh/id_hexuria`, which is why
  pushes work. `gh` also holds `codeitlikemiley` and `hugoforbes88`, which have
  READ — so with either of those active, `gh workflow run`, PR creation and
  anything else token-based fails with `HTTP 403: Must have admin rights`.
  `gh auth switch --user hexuria` fixes it, and is what the active account is
  now set to. The same applies to Claude Code cloud sessions: they authenticate
  through the GitHub integration, never through your local SSH config, so
  connect them while `hexuria` is active or they inherit READ.

- **The name is settled.** `open-ai-gateway` was a deliberate decision, not an
  oversight.

## Deployment paths

| Path | Migrations | Status |
|---|---|---|
| `deploy/compose/stack.yml` | service dependency | works |
| `deploy/helm/` | pre-install/pre-upgrade hook | verified in CI on every relevant push |
| `deploy/tofu/stacks/gcp-cloudrun` | job via `run_execution_token` | validates only |
| `deploy/tofu/stacks/aws-fargate` | container `dependsOn` SUCCESS | validates only |
| `deploy/tofu/stacks/azure-containerapps` | `init_container` | validates only |

No `terraform apply` has ever run against a real account. `validate` proves a
configuration parses against the provider schema — not that a quota exists, that
an IAM binding suffices, or that two resources agree at runtime.
