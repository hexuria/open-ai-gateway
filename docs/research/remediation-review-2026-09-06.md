# Review of the remediation — 2026-09-06

Fourteen agents (two axes × seven PRs) reviewed the stack that closes the
2026-09-05 review. All seven PRs are **green in CI**. This document is the work
that came back, ordered for execution.

Findings marked **[verified]** were re-checked by hand against the tree before
being written down. Everything else is the reviewing agent's claim, credible but
unconfirmed — confirm before acting.

## Decisions taken by the owner, 2026-09-06

These are settled. Do not re-open them.

1. **Thinking budget (B3/B4).** Confirm against the current Anthropic
   documentation for the models on this ladder *before* changing anything. If a
   numeric `budget_tokens` is still accepted, cap it below `max_tokens` and emit
   no thinking block at all for `Off`. If it turns out to be rejected on Claude
   5, **stop and report** — do not build the model-aware renderer on an
   unconfirmed claim.
2. **Helm hooks (D8).** Adopt `helm.sh/hook: pre-install` only, dropping
   `pre-upgrade` — **and add a `helm upgrade` step to `kind-verify.sh`** in the
   same change. The hook change is not to land untested; that is what produced
   the deadlock this review caught.
3. **Counterfactual (B7).** `counterfactual_api_usd` is **zeroed** on
   `abandoned` and `lost` rows, matching `counterfactual_usd`. No answer served,
   no savings booked. The comment at `meter.rs:120-127` already says this and
   stays as written.
4. **Landing.** The eight blockers commit to the PR that introduced them, so
   nothing knowingly broken reaches `main`. Everything else — P1, P2, P3, P4 —
   goes into **one follow-up PR** stacked on `review/group-6-tests-docs`.

## The shape of it

Seven PRs, 123 findings closed. The reviews found **8 blockers**, **9 partial or
incorrect fixes**, **~25 test gaps**, and **~20 false comments**. Two blockers
are regressions the remediation introduced. One is a finding I refuted on a
misreading and must un-refute.

Three patterns account for most of it:

1. **Doc blocks displaced by edits.** Inserting text above an item repeatedly
   landed a doc block on the wrong function, or doubled two `///` lines onto
   one. Six sites. Mechanical to fix, but one of them makes rustdoc say the
   refusal function creates principals.
2. **Tests that pin a helper, not the wiring.** The revert-check caught full
   reverts and missed *partial* ones: delete the call site, keep the helper, and
   the test is still green. Eleven tests.
3. **Fixes that are right in one place and absent in a second.** `U5` covers
   four adapters and misses Codex; `U12` covers refresh and price and misses
   quota; `H2` covers the whole response and half the stream.

---

## P0 — blockers, fix before any merge

### B1. Fargate ALB health-checks a loopback-only listener [verified]
`deploy/tofu/modules/compute-fargate/main.tf:85` — D7 moved the ALB check to
port 8081 `/health/ready`. `crates/oag-core/src/config.rs:107` defaults
`admin_addr` to `127.0.0.1:8081`, and nothing in the Fargate module or stack
sets `OAG_SERVER__ADMIN_ADDR`. Helm (`deployment.yaml:57`) and compose
(`stack.yml:28`) both do. From the ENI the check is refused: every target
unhealthy, ALB serves 503 on a green apply.

**Fix.** Set `OAG_SERVER__ADMIN_ADDR = "0.0.0.0:8081"` in the module's task
definition environment, beside the other `OAG_*` values. Add an assertion to
`deploy/test/tofu-verify.sh`: any module whose health check names 8081 must also
set `ADMIN_ADDR`. That check fails today.

### B2. `redis_private = true` makes the cache unreachable [verified]
`deploy/tofu/modules/data-azure/main.tf:89` creates the private endpoint;
there is no `privatelink.redis.cache.windows.net` zone and no
`private_dns_zone_group`. The `private_dns_zone_id` at `:27` is Postgres's. With
`public_network_access_enabled = false` the hostname still resolves to the now
blocked public IP.

**Fix.** Add the zone, a VNet link, and a `private_dns_zone_group` on the Redis
endpoint, mirroring what Postgres already has.

### B3. `reasoning_effort` renders a budget the upstream rejects [verified]
`Effort::High.as_budget()` is 16384 (`canonical.rs:247`);
`openai::parse_request` defaults `max_tokens` to 4096 (`openai.rs:256`).
Anthropic requires `budget_tokens < max_tokens`, so the finding's own worked
example — `{"model":"oag/auto","reasoning_effort":"high"}` — now 400s where it
previously returned a silent non-thinking answer. `translate-verify.sh` cannot
see this because the mock accepts anything.

**Fix.** Clamp the rendered budget to `max_tokens - 1` (or raise `max_tokens` to
accommodate it — decide which, see D1), and add a case to
`deploy/test/mock-upstream.py` that rejects `budget_tokens >= max_tokens` the
way the real API does, so the harness can see this class at all.

### B4. `Effort::Off` renders `budget_tokens: 0` [verified]
`as_budget(Off) == 0`. `signal()` guards `> 0` (`canonical.rs:349`); neither
renderer does. `reasoning_effort: "none"` / `"minimal"` now sends
`{"type":"enabled","budget_tokens":0}` to Anthropic and `thinkingBudget: 0` to
Gemini. Anthropic's minimum is 1024. **A request that worked before this PR now
fails.** Both review axes found this independently.

**Fix.** `Off` must emit no thinking block at all, in both renderers. The
classifier already treats it that way; make the renderers agree. Test both.

### B5. `init` mints an admin key for a non-admin principal
`crates/oag/src/admin/mod.rs:1174` — `init` calls
`mint_key(db, email, route, "initial", None, true)` with no
`require_admin_principal`, then prints "This is an ADMIN key". After C6,
`upsert_principal` no longer promotes, so for an existing member this is exactly
the C7 failure through a different door. Both axes found it.

**Fix.** Move the check into `mint_key` itself — it already takes `admin: bool`
and already resolves the principal, and the check is currently duplicated across
two `key_cmd` arms and missing from the third. That closes the class rather than
the instance.

### B6. `redact_url` leaks a password in the query string
`crates/oag-core/src/config.rs` — H8's redaction strips userinfo only. libpq
accepts `?user=…&password=…` and `sslpassword=`; that DSN shape prints verbatim
under a subcommand whose help says secrets are redacted. The function doc says
"the first `@`" while its test comment says "LAST possible userinfo boundary" —
and with a raw `@` in a password, `split_once` leaks the tail.

**Fix.** Redact the `password` and `sslpassword` query keys; split the authority
with `rsplit_once`. Test both DSN shapes.

### B7. `counterfactual_api_usd` credits unserved generations
`crates/oag-server/src/gateway/meter.rs:180` sets `counterfactual_api_usd` for
every fate; only `counterfactual_usd` is zeroed (`:139`). Since 0014 makes
`abandoned`/`lost` rows land, `seat_summaries`
(`oag-server/src/admin/mod.rs:326`) and `key_usage` (`repo.rs:1058`, `:1076`)
now credit a seat with the API price of every generation nobody was served —
the effect `meter.rs:120-127` says the baseline must avoid.

**Fix.** Depends on D3 below.

### B8. D23's Azure half is real; my refutation was wrong [verified]
The consolidated table (`review-full-2026-09-05-consolidated.md:194`) says
"missing Azure `LOG_JSON`". I read the full review's looser "missing Azure log
settings" and refuted a diagnostic-settings claim nobody made.
`compute-cloudrun/main.tf:114` sets `OAG_TELEMETRY__LOG_JSON`;
`compute-containerapps/main.tf:106` sets nothing, so Log Analytics receives
unstructured lines.

**Fix.** Set `OAG_TELEMETRY__LOG_JSON=true` on the Container App, and correct
the commit body and PR #69's description, which both assert the wrong verdict.

---

## P1 — fixes that are partial or rest on a false premise

| # | where | what |
|---|---|---|
| C1 | `oag-server/src/gateway/mod.rs:377`, `oag-router/src/ladder.rs:81` | **R1 climbs one rung only.** A ladder `[kimi, kimi-2, anthropic]` with kimi reserve-held still 503s — the finding's own scenario. Disposition *is* consulted and the ledger gate *is* right; the climb is short. The plan's behavioural test was never written (`mod.rs:2186` is a source grep). |
| C2 | `usage/codex.rs:57`, `usage/grok.rs:22`, `usage_poll.rs:67` | **U12 partial.** Refresh and price go through the proxy; **quota does not**. A mandated egress proxy is still bypassed. |
| C3 | `state.rs:187`, `codex.rs` | **U5 partial.** Codex bypasses `normalise_base_url` and still does `format!("{}/responses", base_url)`. |
| C4 | `migrations/0016` | **S6's justification is false.** It says no query filters `schedulable`; `repo::route_channels` (`repo.rs:377`) does. The drop is probably still safe (neither query leads with `provider`, so the partial index was unusable), but the stated reason is wrong and there is no test. |
| C5 | `oag-upstream/src/lib.rs:43-51,512` | **U7's comment is false.** `unwrap_or_default()` → `Client::default()` → `Client::new()` → `.expect(...)`. Same panic, same trigger, and first use *is* on the request path. Both axes found it. |
| C6 | `oag-proto/src/openai.rs:590` | **H2's streamed half rarely fires.** `Refusal` is set only when the refusal delta shares a chunk with `finish_reason`; OpenAI sends `finish_reason` in its own chunk. The test asserts `!= EmptyResponse` rather than `== Refusal` — written around the gap. |
| C7 | `repo.rs:693`, `:1045-1092` | **0015's comment overreaches.** "Nothing rounds on the way out", but `principal_usage` and `key_usage` still cast sums to `::numeric(14,6)` while returning the widened counter beside them. A panel can show a counter disagreeing with its own ledger sum. |
| C8 | `deploy/test/translate-verify.sh:244` | **An assertion that cannot fail.** `grep -q 'POST /v1/messages' mock.log` — the log is cumulative and stage 4 already guaranteed that line. Stage 6 does it correctly with a `wc -l` mark; copy that. |
| C9 | `deploy/test/mock-upstream.py:71` | `MOCK_FAIL_FOR_KEY` reads `x-api-key` or `authorization`, so a bearer credential arrives as `Bearer <token>` and never matches. Works only because Anthropic uses `x-api-key`. Strip the prefix or narrow the docstring. |

---

## P2 — the test gaps

Every one of these is a fix with **no test**, or a test that passes when the fix
is reverted. Grouped by PR; each needs a test that fails on revert.

- **#65** — H4 has no test at all (revert `spawn_unserved` and everything still
  passes); commit `64fc95b`'s `selection_reason` filter has no test.
- **#66** — G4, S1, S9, G2, G3. S1 `EXPLAIN`s an inlined copy of the SQL rather
  than `key_usage`; `principal_usage` has no test. S9's blocked-probe counts any
  backend waiting on a lock, so a sibling test's `db.migrate()` satisfies it.
- **#67** — A4, C8, C9 have no test. Two tests call the helper directly and pass
  with the call site deleted (`lowering_a_budget_…`,
  `an_admin_key_is_refused_…`).
- **#68** — no test: U2, U7, U8, U11, A5/A6, A8, A11, G8's refusal, S6. Passes
  on revert: `a_nonsense_wait_from_redis_…` (reimplements the closure),
  `an_exception_body_that_is_not_utf8_…` (Latin-1 still parses; assert a real
  multibyte sequence).
- **#70** — C8 above; two further assertions are narrow rather than vacuous.

**Approach.** Do not write twenty-five tests mechanically. Take the eleven
revert-passing ones first — those are actively misleading. The "no test at all"
list is mostly comment and metric corrections where a test would be theatre;
apply judgement and say which you skipped and why.

---

## P3 — false comments

One class, ~20 sites. The repo treats these as code.

**Displaced doc blocks** (from inserting text above an item):
`oag/src/admin/mod.rs:1192-1218` (a 20-line block fused onto
`require_admin_principal`; `upsert_principal` and `principal_role` left
undocumented — rustdoc now says the refusal function creates principals);
`oag-server/src/health.rs:22-30`; doubled `///` lines on one line at
`gateway/mod.rs:2183`, `:2217`, `policy.rs:1146`, `oag/src/admin/mod.rs:2230`,
`:2309`, `usage_import.rs:2204`.

**Claims the code does not support:** `oag-core/src/config.rs:573-586`
(`failover_budget` paragraph attached to the wrong `if`; its error names a
conditional that cannot exist); `openai.rs:346` ("the `tool` branch above" — it
is below); `data-neutral/main.tf:44` (tells a self-hosted Redis it "looks like
Upstash"); `data-azure/main.tf:72` and `:76` (cite docs that never mention
`redis_private`); `prometheusrule.yaml:5,10` (claims parity with `alerts.yml`;
names, windows and three rules differ); `values.yaml:514` ("fourteen" → 17);
`gateway/mod.rs:544` (tense); `repo.rs:2571` ("below" → above);
`kind-verify.sh:268-273` (dead check under a stale comment).

**Records:** `review-full-2026-09-05-consolidated.md:99` names
`the_session_timezone_is_utc_whatever_the_server_prefers`; the test is
`…_because_the_client_asked` (`db.rs:215`).

---

## P4 — bookkeeping

- **D13: the `v0.1.0` tag was never published.** `git tag -l` and
  `git ls-remote --tags origin` are both empty, and `Chart.yaml:6` says
  `appVersion: 0.1.0`, so the documented `helm install` lands on
  `ImagePullBackOff`. Half of D13 is genuinely outstanding.
- **C17 is in no group table.** The #66 reviewer says the CLI half of S2 closes
  it incidentally. Confirm, then either claim it or add it to a group.
- **Group 4's row writes its scope as "A5–A11", which sweeps in A7** — A7
  belongs to Group 2 (line 67) and was done there in `81466c7`. One-character
  fix in `.claude/commands/remediate.md`.
- **`breaker-verify.sh:61`** — `grep -c . || echo 0` emits `0\n0` on an empty
  list; use `|| true`.
- **`compute-cloudrun/variables.tf:96`** — `deletion_protection` is a variable
  no stack passes, which is the exact shape this PR's own `tofu-verify.sh` calls
  dead code. Either pass it or drop the variable.

---

## Execution order for Opus 5

### Stage 1 — the eight blockers, onto their own branches

Each commits to the PR that introduced it. Every one needs a test that fails
when the fix is reverted, and the revert must be *partial* as well as total:
delete the call site, keep the helper, and confirm the test still fails. That
distinction is what this review found eleven of my tests missing.

| order | branch | items |
|---|---|---|
| 1 | `review/group-1-proto` (#71) | **B3, B4** — one change to effort rendering in both renderers. Read the Anthropic docs first, per decision 1. Teach `mock-upstream.py` to reject `budget_tokens >= max_tokens` and `budget_tokens < 1024`, so the harness can see this class at all; today it accepts anything. |
| 2 | `review/group-3-operator` (#67) | **B5, B6** — put the admin check inside `mint_key`, which closes the class rather than the instance; redact the DSN query keys and switch to `rsplit_once`. |
| 3 | `review/group-0-ledger-key` (#65) | **B7** — zero `counterfactual_api_usd` on unserved fates, plus the gated store test that catches it. Also closes the H4 test gap listed in P2. |
| 4 | `review/group-5-deploy` (#69) | **B1, B2, B8** — all deploy. None is testable without a cloud account, so lean on `tofu-verify.sh`: add an assertion that a module health-checking 8081 must set `ADMIN_ADDR` (fails today), and one that a private endpoint has a DNS zone group. |

Then re-run CI bottom-first and merge the stack in order.

### Stage 2 — one follow-up PR on `review/group-6-tests-docs`

Branch `review/group-7-review-fixes`. Order within it:

1. **D8** — `pre-install` only, **with** the `helm upgrade` step added to
   `kind-verify.sh` in the same commit (decision 2).
2. **C1–C9**, in PR order.
3. **P2**, eleven revert-passing tests first; the "no test at all" list second,
   skipping any where a test would be theatre and saying which and why.
4. **P3** as one commit per PR's worth of comments — mechanical, easier to
   review together than scattered.
5. **P4** last.
