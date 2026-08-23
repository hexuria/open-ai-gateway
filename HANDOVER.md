# Handover

State of `open-ai-gateway` as of 2026-08-23, written to be picked up on another
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

Green: 282 tests, clippy clean at `-D warnings`, fmt clean. CI and the release
workflow both pass on `main`. `ghcr.io/hexuria/open-ai-gateway:main` is published
for linux/amd64 and linux/arm64 and is publicly pullable.

All work is on `main` — there is no open branch and no PR to merge. That was the
pattern from the start, when the repo was published straight to `hexuria`.

```bash
just verify        # request path end to end vs a mock upstream, ~1 min, no credentials
just verify-k8s    # rolling restart severs no stream; needs kind, ~10 min
just check         # fmt + clippy + tests
just dev-serve     # local gateway; also what the editor run button launches
```

`just verify-k8s` also runs in CI (`.github/workflows/k8s.yml`) on every push
touching `crates/`, `deploy/` or `migrations/`, on PRs, and nightly. As of
2026-08-23 it passes: 8 of 8 streams complete across a full `rollout restart`
and all 8 reach the ledger. That claim used to be an anecdote from one manual
check; it is now checked by a runner.

`just verify` is the one to run first on a new machine. It passes today with:

```
anthropic/claude-haiku-4.5 on 'cheap': in=100 out=18,
$0.00019000 vs $0.00285000 frontier — 93% saved, ttft 998ms
```

## Pick up here

In priority order. Everything here is blocked on credentials or on a decision —
the local and CI verification is done.

### 1. Point it at one real credential

Nothing has ever talked to a real provider. The adapters, credential unsealing
and the SigV4 path have only ever run against the mock upstream.

```bash
just dev-serve
oag admin add-account --name anthropic-1 --provider anthropic --secret sk-...
```

Then send a request through `oag/auto` and read the ledger row. This also
unblocks three things that cannot move without it:

- **Bedrock**: the event-stream decoder was verified against an encoder written
  in the same commit, which proves self-consistency and nothing about AWS's
  actual framing.
- **OAuth**: the two-layer refresh (process mutex, fleet lock, version stamp,
  `invalid_grant` recovery) is the most concurrency-sensitive code in the repo
  and has only unit coverage.
- **`count_tokens` calibration**: the divisors in `oag_proto::count_input_tokens`
  are reasoned, not measured. Run five real prompts through Anthropic's own
  `count_tokens` and adjust. Until then the `oag_estimate: true` flag is doing
  real work.

### 2. Run `deploy/tofu/verify-migration-gate.sh` once per cloud

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

- **Circuit breakers** are wired and unit-tested but never exercised end to end.
  The mock has `MOCK_FAIL_STATUS` and `MOCK_FAIL_FIRST` for exactly this, and
  `deploy/test/kind-verify.sh` is now a working template for that kind of test.
- **In-cluster Postgres** is a single StatefulSet with no operator, no PITR and
  no pooling. Fine for kind; wrong for the credential store. Use CloudNativePG
  with `data.mode=external`.
- **`/v1/models` in passthrough** returns the ladder plus every off-ladder
  catalog model, rendered per request. The built-in catalog is small; the
  documented seeding path is LiteLLM's table, which is >1000 entries. Memoise
  against the catalog `Arc` if that bites — do not cap the list, which would
  make the answer wrong rather than large.
- **`route_providers` alias asymmetry**: `Provider::from_str` accepts aliases but
  `select::lease` queries the canonical spelling, so an account row spelled
  `moonshot` is advertised by `/v1/models` and never actually selectable.
  Normalise on write or filter the listing.

## Things that will bite if you do not know them

- **The Cloud Run stack's plan is never clean.** The API does not return
  `run_execution_token`, so it re-diffs every plan. That is what makes the
  migration execute. Do not silence it with `ignore_changes`, and do not use
  `terraform plan -detailed-exitcode` as a drift gate there.
- **Migrations must be expand/contract.** In all three clouds the migration lands
  while the previous release is still serving — on AWS for up to the 1800s
  deregistration delay.
- **`migrations/0001_baseline.sql` is edited in place**, not appended to. That is
  only safe while no database anyone cares about has applied it. The moment there
  is a real deployment, stop: sqlx checksums applied migrations and `oag migrate`
  will fail closed, permanently, against a database that ran an earlier version
  of `0001`. Changes become `0002` from then on.
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
