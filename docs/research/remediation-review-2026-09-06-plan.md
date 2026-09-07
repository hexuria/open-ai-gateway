# Execution plan: fixing what the review of the remediation found

Companion to `remediation-review-2026-09-06.md` (the findings) — this is the
*how*. Written so a workhorse model can execute it without re-deriving
anything. Every item has the same six parts:

```
WRONG      what the defect is, in one or two sentences
WHERE      branch, file, and a grep anchor (line numbers drift; anchors do not)
PROVE IT   a command that demonstrates the defect BEFORE you change anything
FIX        the exact change
TEST       the test that fails without the fix — and the partial-revert check
VERIFY     the command that proves it is fixed
```

**Do not skip PROVE IT.** Three findings in the original review were refuted on
a misreading and one refutation turned out wrong. Seeing the defect first is
the cheapest insurance there is.

---

## 0. Ground rules (read once, obey throughout)

### The environment

```bash
cd /Volumes/goldcoders/OSS/open-ai-gateway

# Postgres 5452 and Redis 6399 are already up. The `oag` database on 5452 is
# LIVE — a gateway serves opengrok from it and its storage is tmpfs. Never
# migrate, ALTER, drop or reset it. Tests and verify scripts use oag_g0:
export OAG_TEST_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/oag_g0
export OAG_TEST_REDIS_URL=redis://127.0.0.1:6399
export OAG_DATABASE__URL=postgres://oag:oag@127.0.0.1:5452/oag_g0
export OAG_REDIS__URL=redis://127.0.0.1:6399
```

- Never `just dev-down`, `just dev-reset`, `DROP DATABASE`, `CREATE DATABASE`.
- Never paste a key, token or password into a message, commit, PR body or log.
- `gh` is logged in as `hexuria`, which can open and edit PRs. Check with
  `gh auth status` before any PR operation.
- Stray `target/debug/oag serve` processes accumulate when a verify script is
  killed. Pid **98283** is the live opengrok gateway — never touch it. Kill only
  processes whose `etime` is in minutes (`MM:SS`, one colon):
  ```bash
  ps -eo pid,etime,command | grep '[t]arget/debug/oag serve' \
    | awk '$2 ~ /^[0-9]+:[0-9]+$/ && $1 != 98283 {print $1}' | xargs -r kill -9
  ```

### The stack

```
main
 └─ review/group-0-ledger-key   #65
     └─ review/group-1-proto    #71
         └─ review/group-2-ledger    #66
             └─ review/group-3-operator   #67
                 └─ review/group-4-upstream   #68
                     └─ review/group-5-deploy   #69
                         └─ review/group-6-tests-docs   #70
                             └─ review/group-7-review-fixes   (Stage 2, new)
```

Stage 1 commits land on the branch named in each item. **After committing to a
lower branch, merge it upward** so the stack stays consistent:

```bash
# after committing to review/group-1-proto, for example:
for b in group-2-ledger group-3-operator group-4-upstream group-5-deploy group-6-tests-docs; do
  git checkout review/$b && git merge --no-edit review/$(prev) && git push
done
```
(Write the loop out explicitly with the real sequence; `prev` is shorthand.)

### The gate — run before every push, nothing piped through `tail`

```bash
cargo fmt --all
just check                                  # fmt, pedantic clippy -D warnings, all tests
cargo test -p oag-store                     # gated: must say 48 passed, and NO "skipped:" line
./deploy/test/helm-render-verify.sh         # if deploy/helm touched
./deploy/test/tofu-verify.sh                # if deploy/tofu touched
terraform fmt -check -recursive deploy/tofu # if deploy/tofu touched
# if the request path or a verify script is touched:
./deploy/test/local-verify.sh
./deploy/test/translate-verify.sh
./deploy/test/dialects-verify.sh
./deploy/test/bedrock-verify.sh
./deploy/test/breaker-verify.sh
```

### The revert-check — what "fails without the fix" means here

The original remediation revert-checked every test and still shipped eleven
that pass with the fix reverted. The gap was **partial reverts**. A test passes
the check only if it fails under BOTH of these:

1. **Total revert** — `git stash` the fix, run the test, `git stash pop`.
2. **Partial revert** — keep the helper, delete only the *call site* that wires
   it in. If the test still passes, it is testing the helper, not the fix.

Do both. Report both. "Fails on total revert, passes on partial" is a defect in
the test, not a pass.

### Commit conventions

One commit per item. Subject prefixed by area (`proto:`, `gateway:`, `store:`,
`server:`, `cli:`, `upstream:`, `tofu:`, `helm:`, `deploy:`, `test:`, `docs:`).
Body: what was wrong, why the fix is right, how it was verified. End with:

```
Claude-Session: https://claude.ai/code/session_01MzMCaSE5rk4wt2Qx326QbF
```

A comment the fix makes false is fixed in the same commit.

---

## Stage 1 — eight blockers, onto the branches that introduced them

Order matters only for the upward merges. Do them in this sequence.

### B3 + B4 — `reasoning_effort` renders a budget the upstream rejects

**Branch:** `review/group-1-proto`

**Owner decision (settled):** confirm against the current Anthropic docs
first. If a numeric `budget_tokens` is still accepted on the ladder's models,
clamp it; if it is rejected on Claude 5, **stop and report** — do not build a
model-aware renderer.

**WRONG.** Two things, both certain:
- `Effort::High.as_budget()` is 16384 and `openai::parse_request` defaults
  `max_tokens` to 4096. Anthropic requires `budget_tokens < max_tokens`. So the
  finding's own example, `{"model":"oag/auto","reasoning_effort":"high"}`,
  now 400s where it used to return a non-thinking answer.
- `Effort::Off.as_budget()` is 0, and neither renderer guards it. `"none"` /
  `"minimal"` send `{"type":"enabled","budget_tokens":0}` (minimum 1024) and
  `thinkingBudget: 0`. Requests that worked before this branch now fail.

**WHERE.**
- `crates/oag-proto/src/canonical.rs` — `fn as_budget` (Off → 0, High → 16384)
- `crates/oag-proto/src/anthropic.rs` — anchor `.or_else(|| req.thinking_effort.map(Effort::as_budget))`, then `body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget })`
- `crates/oag-proto/src/gemini.rs` — same anchor, then `json!({ "thinkingBudget": budget })`
- `crates/oag-proto/src/openai.rs` — anchor `.unwrap_or(4096)` in `parse_request`

**PROVE IT.**
```bash
git checkout review/group-1-proto
grep -n "Self::Off => 0\|Self::High => 16384" crates/oag-proto/src/canonical.rs
grep -n "unwrap_or(4096)" crates/oag-proto/src/openai.rs
grep -n 'budget_tokens": budget' crates/oag-proto/src/anthropic.rs   # no guard around it
```
Then the docs check the owner asked for:
```
WebFetch https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking
Question: for claude-opus-5 and claude-sonnet-5, is `thinking: {type: "enabled",
budget_tokens: N}` accepted, and what are the constraints on N relative to
max_tokens? Is there a newer "adaptive"/"effort" form these models require?
```
Record the answer in the commit body. **If `budget_tokens` is rejected on the
Claude 5 models, STOP HERE and report; do not proceed with the clamp.**

**FIX** (assuming `budget_tokens` is accepted). In *both* renderers, replace the
bare `.or_else(...)` with a helper on `CanonicalRequest` so the rule lives once:

```rust
// canonical.rs, on impl CanonicalRequest
/// The thinking budget to put on the wire, or `None` for no thinking block.
///
/// `Off` is a request for no thinking, not a request for zero tokens of it —
/// the classifier already reads it that way (`signal()` guards `> 0`), and a
/// renderer that sent `budget_tokens: 0` was refused by Anthropic (min 1024).
///
/// Clamped below `max_tokens` because Anthropic requires it; a Chat
/// Completions client leaves `max_tokens` at 4096 and `High` asks for 16384.
pub fn wire_thinking_budget(&self) -> Option<u32> {
    let asked = self
        .thinking_budget
        .filter(|b| *b > 0)
        .or_else(|| self.thinking_effort.map(Effort::as_budget).filter(|b| *b > 0))?;
    let ceiling = self.max_tokens.saturating_sub(1);
    Some(asked.min(ceiling))
}
```
Check the field name for `max_tokens` on `CanonicalRequest` (grep `pub max_tokens`) and its type; adapt the `saturating_sub` accordingly. Anthropic's minimum is 1024 — if `ceiling < 1024`, return `None` (no block) rather than an invalid block; add that branch and a comment.

Then in `anthropic.rs` and `gemini.rs`:
```rust
if let Some(budget) = req.wire_thinking_budget() {
    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });   // anthropic
    // gemini: body["generationConfig"]["thinkingConfig"] = json!({ "thinkingBudget": budget });
}
```
Fix the comment at the anthropic/gemini anchors so it describes the clamp.

**TEST.** In `canonical.rs` tests, and one each in `anthropic.rs` / `gemini.rs`:
- `effort_high_is_clamped_below_max_tokens` — `max_tokens: 4096`,
  `thinking_effort: Some(High)` → rendered budget is `4095`, not 16384.
- `effort_off_renders_no_thinking_block` — `thinking_effort: Some(Off)` →
  `body["thinking"]` is `Null` (anthropic) / `thinkingConfig` absent (gemini).
- `a_ceiling_under_the_minimum_renders_no_block` — `max_tokens: 512`,
  `High` → no block.

Partial-revert check: replace `wire_thinking_budget()` in *one* renderer with
the old `.or_else` expression; that renderer's tests must fail.

Also teach the mock what the real API does, so the harness can see this class:
in `deploy/test/mock-upstream.py` `do_POST`, before serving, if
`request.get("thinking", {}).get("type") == "enabled"`:
`bt = request["thinking"]["budget_tokens"]`; if `bt >= request.get("max_tokens", 4096)` or `bt < 1024`, respond 400 with `{"type":"error","error":{"type":"invalid_request_error","message":"budget_tokens must be >= 1024 and < max_tokens"}}`. Comment why.

**VERIFY.**
```bash
cargo test -p oag-proto
./deploy/test/translate-verify.sh     # stage 6 sends reasoning_effort:high; must still PASS
```
Before the fix, with the mock change applied, `translate-verify.sh` stage 6
**fails** (the mock now 400s the 16384-vs-4096 request). After: passes. That is
the end-to-end proof.

---

### B5 + B6 — `init` mints an admin key it should refuse; `redact_url` leaks

**Branch:** `review/group-3-operator`

#### B5

**WRONG.** C6 made `upsert_principal` stop promoting an existing member to
admin. But `init` then calls `mint_key(..., admin = true)` with no
`require_admin_principal`, and prints "This is an ADMIN key". For an existing
non-admin principal that is exactly the half-privileged key C7 refuses in
`key create --admin`. The check is also duplicated across two `key_cmd` arms
and absent from this third caller.

**WHERE.** `crates/oag/src/admin/mod.rs`
- `async fn init(` → anchor `mint_key(db, email, route, "initial", None, true)`
- `async fn mint_key(` — takes `admin: bool`, already resolves the principal
- the two `if admin { require_admin_principal(db, &email).await?; }` in `key_cmd`

**PROVE IT.**
```bash
git checkout review/group-3-operator
grep -n 'mint_key(db, email, route, "initial", None, true)' crates/oag/src/admin/mod.rs
grep -n 'require_admin_principal' crates/oag/src/admin/mod.rs   # note: not before that line
```

**FIX.** Move the check *into* `mint_key`: at the top of `mint_key`, after the
principal is resolved, `if admin { require_admin_principal(db, email).await?; }`.
Delete the two now-redundant checks in `key_cmd`. In `init`, the practical
consequence is that `init` on an existing non-admin email now errors with the
message that names `oag admin principal promote` — which is the right outcome;
make sure `init`'s own error path prints it cleanly rather than a bare `?`.

**TEST.** `init_refuses_an_admin_key_for_an_existing_member` (gated, in the
`mod tests` beside `an_admin_key_is_refused_for_a_principal_who_is_not_one`):
create a principal with role `member`, call `init` for that email, assert
`Err` whose message contains `principal promote`, and assert **no key row was
inserted** (`SELECT count(*) FROM api_key WHERE principal_id = $1` is 0).

Partial-revert check: delete only the `if admin { … }` inside `mint_key`; the
test must fail. Also fix `an_admin_key_is_refused_for_a_principal_who_is_not_one`
so it goes through `mint_key`, not `require_admin_principal` directly (P2).

**VERIFY.** `cargo test -p oag init_refuses` and `cargo test -p oag an_admin_key`.

#### B6

**WRONG.** `redact_url` strips userinfo only. libpq accepts
`postgres://host/db?user=u&password=p` and `sslpassword=`; that DSN prints the
password verbatim under `oag config`, whose help says secrets are redacted.
Separately, the function splits on the *first* `@` while its test comment
claims the last; a raw `@` inside a password leaks the tail.

**WHERE.** `crates/oag-core/src/config.rs` — `fn redact_url`, anchor
`rest.split_once('@')`.

**PROVE IT.**
```bash
grep -n "split_once('@')" crates/oag-core/src/config.rs
grep -n "password=\|sslpassword" crates/oag-core/src/config.rs    # nothing handles it
```

**FIX.** In `redact_url`:
1. Split scheme, then split the remainder at the first `/` or `?` into
   authority and tail. In the authority, `rsplit_once('@')` — the *last* `@`
   is the userinfo boundary; everything before it is replaced with `***`.
2. In the tail, if there is a query string, rewrite any `password=` and
   `sslpassword=` values to `***` (split on `&`, `split_once('=')`, compare key
   case-sensitively, rejoin).
Fix the doc comment ("the first `@`") and the test comment to agree.

**TEST.** Extend `a_printed_config_carries_no_password`:
- `postgres://host:5432/db?user=u&password=hunter2&sslmode=require` → output
  contains `password=***`, does not contain `hunter2`, still contains
  `sslmode=require`.
- `postgres://u:p@ss@host/db` → output does not contain `p@ss`, does not
  contain `ss@host` (the first-`@` leak).
Partial-revert: restore `split_once` — the second case must fail; remove the
query rewrite — the first must fail.

**VERIFY.** `cargo test -p oag-core redact`.

---

### B7 — savings booked for answers nobody received

**Branch:** `review/group-0-ledger-key`

**Owner decision (settled):** zero `counterfactual_api_usd` on `abandoned` and
`lost` rows, matching `counterfactual_usd`. The comment at the top of the
`Fate` handling already states this rule; it stays.

**WRONG.** Since 0014 makes unserved rows land, `record_with_gate` writes
`counterfactual_api_usd: api_equivalent` for every fate while
`counterfactual_usd` is zeroed for `Abandoned | Lost`. `seat_summaries` and
`key_usage` sum `counterfactual_api_usd`, so a seat is credited with the API
price of every generation nobody was served.

**WHERE.** `crates/oag-server/src/gateway/meter.rs`, in `record_with_gate`:
- anchor `let api_equivalent = ctx.decision.model.pricing.cost(&usage);`
- anchor `Fate::Abandoned | Fate::Lost => (Decimal::ZERO, None),`
- anchor `counterfactual_api_usd: api_equivalent,`

**PROVE IT.**
```bash
git checkout review/group-0-ledger-key
grep -n "counterfactual_api_usd: api_equivalent" crates/oag-server/src/gateway/meter.rs
grep -n "Fate::Abandoned | Fate::Lost => (Decimal::ZERO" crates/oag-server/src/gateway/meter.rs
```
One is zeroed by fate; the other is not.

**FIX.** Make `api_equivalent` follow the same fate match:
```rust
let api_equivalent = match fate {
    Fate::Served => ctx.decision.model.pricing.cost(&usage),
    // No answer was served, so no API spend was displaced. The other
    // counterfactual already says so; this one was left saying the opposite,
    // and seat summaries booked the API price of every abandoned generation
    // as savings.
    Fate::Abandoned | Fate::Lost => Decimal::ZERO,
};
```

**TEST.** In `meter.rs` tests, beside the existing one that asserts
`row.counterfactual_usd == Decimal::ZERO` for a lost row: add
`an_unserved_row_displaces_no_api_spend` asserting
`row.counterfactual_api_usd == Decimal::ZERO` for both `Fate::Lost` and
`Fate::Abandoned`, and `> ZERO` for `Fate::Served` with the same usage.

Then the gated store test that catches the *symptom*: in `repo.rs`, insert one
served row and one lost row for the same seat account with nonzero usage, and
assert `seat_summaries` (or whichever function feeds the admin seat table —
grep `counterfactual_api_usd` in `crates/oag-server/src/admin/mod.rs` to find
the query and its repo entry point) reports the served row's API price only.

**VERIFY.** `cargo test -p oag-server meter` and the gated store run. This
also closes the H4 test gap from P2: while here, add a test that
`spawn_unserved` is actually called on the error paths — the partial-revert
target is deleting the `spawn_unserved(...)` call in `run_with_escalation`.

---

### B1 + B2 + B8 — deploy blockers

**Branch:** `review/group-5-deploy`. None is testable without a cloud account.
Lean on `deploy/test/tofu-verify.sh` assertions plus `terraform validate`.

#### B1 — Fargate health-checks a loopback-only listener

**WRONG.** D7 moved the ALB health check to `port = "8081"`,
`path = "/health/ready"`. The gateway's `admin_addr` defaults to
`127.0.0.1:8081`. Helm and compose both set `OAG_SERVER__ADMIN_ADDR=0.0.0.0:8081`;
the Fargate module and stack do not. From the ENI the check is refused: every
target unhealthy, ALB serves 503, on a green apply.

**WHERE.**
- `deploy/tofu/modules/compute-fargate/main.tf` — anchor `port                = "8081"` in `health_check`; anchor `container_env = [for k, v in var.env : { name = k, value = v }]`
- `crates/oag-core/src/config.rs` — anchor `admin_addr: "127.0.0.1:8081"`

**PROVE IT.**
```bash
git checkout review/group-5-deploy
grep -rn "ADMIN_ADDR" deploy/tofu/          # nothing
grep -n 'admin_addr: "127.0.0.1:8081"' crates/oag-core/src/config.rs
grep -n 'port                = "8081"' deploy/tofu/modules/compute-fargate/main.tf
```

**FIX.** In the module (it owns the health-check port, so it owns the
listener it depends on): build `container_env` from `merge(var.env, { OAG_SERVER__ADMIN_ADDR = "0.0.0.0:8081" })` — with `var.env` merged *last* is wrong; the module's requirement must win, so put the module's map second in `merge()`. Comment: the ALB check arrives on the ENI, the default bind is loopback, and the container's own liveness check on 8080 never noticed because it curls 127.0.0.1. Note the security posture is unchanged: 8081 is reachable only from the ALB security group (D7 added those rules).

**TEST.** In `deploy/test/tofu-verify.sh`, a third check: for every module under `deploy/tofu/modules/`, if any `health_check` block or `health_check_config` names port 8081, the same module must contain the literal `OAG_SERVER__ADMIN_ADDR`. Write it in the same Python style as the two existing checks, with a comment naming this defect. **It fails on the branch before the fix** — run it first to see.

**VERIFY.** `./deploy/test/tofu-verify.sh` (three lines of `tofu:`), `terraform -chdir=deploy/tofu/modules/compute-fargate validate`, `terraform fmt -check -recursive deploy/tofu`.

#### B2 — `redis_private = true` makes the cache unreachable

**WRONG.** The private endpoint is created, but there is no
`privatelink.redis.cache.windows.net` DNS zone, no VNet link, and no
`private_dns_zone_group` on the endpoint. With public access off, the hostname
still resolves to the public IP that is now blocked. Every replica loses Redis
on a green apply. (Postgres's `private_dns_zone_id` at the top of the file is
Postgres's zone, not a Redis one.)

**WHERE.** `deploy/tofu/modules/data-azure/main.tf` — anchor
`resource "azurerm_private_endpoint" "redis"`; `variables.tf` for
`private_endpoint_subnet_id` and whatever VNet id is available.

**PROVE IT.**
```bash
grep -n "privatelink.redis\|private_dns_zone_group" deploy/tofu/modules/data-azure/main.tf   # nothing
grep -n "private_dns_zone" deploy/tofu/modules/data-azure/main.tf   # only Postgres's
```

**FIX.** Guarded by `count = var.redis_private ? 1 : 0`:
```hcl
resource "azurerm_private_dns_zone" "redis" {
  name                = "privatelink.redis.cache.windows.net"
  resource_group_name = var.resource_group_name
}
resource "azurerm_private_dns_zone_virtual_network_link" "redis" {
  name                  = "${var.name}-redis"
  resource_group_name   = var.resource_group_name
  private_dns_zone_name = azurerm_private_dns_zone.redis[0].name
  virtual_network_id    = var.virtual_network_id   # add the variable if absent; see how Postgres's zone is linked
}
```
and on `azurerm_private_endpoint.redis`:
```hcl
private_dns_zone_group {
  name                 = "redis"
  private_dns_zone_ids = [azurerm_private_dns_zone.redis[0].id]
}
```
Look at how the Postgres zone is created/linked in the azure stack and mirror
that exactly (the stack may own the zone rather than the module — follow the
existing pattern, don't invent a second one). Comment: without the zone the
name resolves publicly and the private endpoint is decoration.

**TEST.** `tofu-verify.sh`, fourth check: every `azurerm_private_endpoint`
resource must contain a `private_dns_zone_group` block. Fails before, passes
after.

**VERIFY.** `tofu-verify.sh`, `terraform -chdir=deploy/tofu/stacks/azure-containerapps validate` (after `init -backend=false`), `fmt -check`.

#### B8 — Azure `LOG_JSON` is missing; the earlier "not reproduced" was wrong

**WRONG.** D23's fourth item (consolidated table, line 194) is "missing Azure
`LOG_JSON`". `compute-cloudrun` sets `OAG_TELEMETRY__LOG_JSON`; Container Apps
sets nothing, so Log Analytics receives unstructured lines. The commit body of
`15afc58` and PR #69's description both assert the finding was not reproduced.
Both are wrong.

**WHERE.**
- `deploy/tofu/modules/compute-containerapps/main.tf` — anchors `name  = "OAG_SERVER__PUBLIC_ADDR"` (two `env` blocks: the app and the migrate job)
- `deploy/tofu/modules/compute-cloudrun/main.tf` — anchor `name  = "OAG_TELEMETRY__LOG_JSON"` (the pattern to copy)

**PROVE IT.**
```bash
grep -rn "LOG_JSON" deploy/tofu/modules/    # cloudrun only
sed -n 194p docs/research/review-full-2026-09-05-consolidated.md
```

**FIX.** Add `OAG_TELEMETRY__LOG_JSON = "true"` beside `PUBLIC_ADDR` in the
Container App's env block (and the migrate job's, if it has one). Then correct
the record: in `deploy/helm/open-ai-gateway`… no — in the **PR #69 body**, replace
the "One finding not reproduced" section with a "One finding I misread" section
stating this; use `gh pr edit 69 --body-file`. The commit body of `15afc58`
cannot be rewritten (it is pushed and reviewed); state the correction in this
commit's body instead.

**TEST.** `tofu-verify.sh`, fifth check: every compute module must set
`OAG_TELEMETRY__LOG_JSON`. (Fargate: check whether it does; if not, add it
there too — same finding.)

**VERIFY.** `tofu-verify.sh`, `validate`, `fmt -check`.

---

### Stage 1 close-out

1. Merge upward so every branch contains every fix below it (see §0).
2. Push all seven branches.
3. Wait for CI on all seven. `drain` takes ~10 minutes. Nothing merges red.
4. Update each touched PR's body with a short "Post-review fixes" section
   listing the B-items landed there (`gh pr edit N --body-file`).
5. Report: per item, the PROVE IT output before, the test name, the two
   revert-check outcomes, the VERIFY output after.

---

## Stage 2 — one follow-up PR: `review/group-7-review-fixes`

```bash
git checkout review/group-6-tests-docs && git pull
git checkout -b review/group-7-review-fixes
```

Order within the PR: **D8 → C1..C9 → P2 → P3 → P4.**

### D8 — Helm hooks: install-only, plus an upgrade test

**Owner decision (settled):** `helm.sh/hook: pre-install` only, dropping
`pre-upgrade` — and a `helm upgrade` step in `kind-verify.sh` in the same
commit. Hook changes do not land untested again.

**WRONG.** With `pre-install,pre-upgrade` and Helm's default
`before-hook-creation` policy, every `helm upgrade` deletes and recreates both
data StatefulSets and their Services before the new pods exist: 20–40 s with no
database, on an upgrade that reports success. Dropping the hooks entirely
deadlocks the install (the migrate Job is a pre-install hook whose init
container waits for this Postgres). Install-only keeps the ordering and stops
the upgrade-time recreate.

**WHERE.** `deploy/helm/open-ai-gateway/templates/data-incluster.yaml` — four
anchors `"helm.sh/hook": pre-install,pre-upgrade`. `deploy/test/kind-verify.sh`
— anchor `say "4/7  install the chart"` and `say "6/7  open $STREAMS streams`.

**PROVE IT.**
```bash
grep -n 'helm.sh/hook": pre-install,pre-upgrade' deploy/helm/open-ai-gateway/templates/data-incluster.yaml   # 4 hits
grep -n "helm upgrade" deploy/test/kind-verify.sh    # nothing — the D8 scenario is untested
```

**FIX.**
1. All four annotations → `"helm.sh/hook": pre-install`. Rewrite the header
   comment: the premise paragraph stays; replace the "remedy unavailable"
   reasoning with: install-only preserves ordering and stops the upgrade-time
   recreate; the cost is that the data tier's spec is frozen after install —
   a change to Postgres's image or resources needs a manual apply — and that
   is acceptable for a tier the chart says is for a demo or a single node.
2. In `kind-verify.sh`, a new stage between install and the streams: record
   the Postgres pod's UID (`$KC get pod -l app.kubernetes.io/name=…-postgres -o jsonpath='{.items[0].metadata.uid}'`), run `helm upgrade` with the same values plus one harmless change (e.g. `--set podAnnotations.upgrade=1` — check `values.yaml` for an existing annotation hook; add one if needed), wait for rollout, then assert the Postgres pod UID is **unchanged** and the ledger row count did not reset. Comment: this is the D8 scenario, and it was never exercised. Renumber the stages.

**TEST.** The new stage *is* the test. Revert-check: put `pre-upgrade` back
and run `kind-verify.sh` — the UID assertion must fail. (Local runs of
`kind-verify.sh` died of memory on the owner's machine; this may have to be
proven in CI — say so explicitly if so, and push a throwaway commit with
`pre-upgrade` restored to see the red run, then the real one.)

**VERIFY.** `helm-render-verify.sh`; CI `drain` green.

---

### C1 — R1 climbs one rung only

**WRONG.** The finding: "a *higher* rung naming a different provider could have
served". The fix consults `disposition()` (correct) and climbs exactly one rung,
giving up if that rung is the same provider. Ladder `[kimi, kimi-2, anthropic]`
with kimi reserve-held still 503s. The plan's behavioural test was never
written; the existing test is a source grep.

**WHERE.** `crates/oag-server/src/gateway/mod.rs` in `run_with_escalation` —
anchor `if matches!(e.disposition(), oag_core::Disposition::EscalateTier)` and
the comment above it `// Only to a rung naming a *different* provider`.
`crates/oag-router/src/ladder.rs` for rung iteration (`pub fn rungs`).

**PROVE IT.** Read the block; note it computes one `next` rung and bails when
`next.provider == current.provider`. Then write the test below first and watch
it fail.

**FIX.** Iterate: from the current rung upward, skip rungs whose every model
is the same provider as the one that failed; stop at the first rung naming a
different provider; if none, return the original error. Keep `MAX_ESCALATIONS`
semantics for *quality* climbs separate — this is a *credential* fall-through,
and the comment should say why it is allowed to pass several rungs when a
quality climb is not (a rung with no credential contributes nothing; skipping
it is not escalating past it).

**TEST.** Behavioural, using the `state()` harness that already exists in
`mod.rs` tests (grep `fn state(` under `mod router_tests`): a three-rung ladder
`[kimi, kimi-2, anthropic]` where kimi's only credential is reserve-held; a
request with `oag/auto` must be served by the anthropic rung, and the ledger
row's `escalation_gate` must be `NoCredential`. Delete the source-grep test.
Partial-revert: restore the single-step version — the test must fail.

**VERIFY.** `cargo test -p oag-server reserve_held` (name the test so).

---

### C2 — quota calls bypass the egress proxy (U12 partial)

**WRONG.** `proxy_url` is applied to inference, refresh and price. Quota
polling still builds proxy-less clients.

**WHERE.**
- `crates/oag-upstream/src/usage/codex.rs` — anchor `let client = reqwest::Client::builder()`
- `crates/oag-upstream/src/usage/grok.rs` — same anchor
- `crates/oag-server/src/usage_poll.rs` — anchor `.served_models(material, row.proxy_url.as_deref())` (this one passes it) vs the quota call beside it (find it: grep `usage::` or `quota` in that file)

**PROVE IT.** `grep -n "proxy" crates/oag-upstream/src/usage/codex.rs crates/oag-upstream/src/usage/grok.rs` — nothing.

**FIX.** Route both through `side_channel_client(proxy_url)` (grep `pub fn side_channel_client` in `oag-upstream/src/lib.rs` — it exists from U12), thread `proxy_url: Option<&str>` into the two `fetch` functions, and pass `row.proxy_url.as_deref()` from `usage_poll.rs`.

**TEST.** Unit: each `fetch` builds its client via `side_channel_client` —
test by pointing `proxy_url` at a local listener that records a CONNECT and
asserting it was hit (there is precedent in the U12 refresh test; copy it).
Partial-revert: replace one call with `Client::builder()` — its test fails.

**VERIFY.** `cargo test -p oag-upstream usage`.

---

### C3 — Codex bypasses base-URL normalisation (U5 partial)

**WRONG.** `state.rs` passes `cx.base_url` straight to `with_base_url`; `codex.rs`
still does `format!("{}/responses", self.base_url)`. A trailing slash yields
`//responses`.

**WHERE.** `crates/oag-server/src/state.rs` — anchor `.with_base_url(cx.base_url.clone())`; `fn normalise_base_url(provider, raw)` above it. `crates/oag-upstream/src/codex.rs` — anchors `format!("{}/responses", self.base_url)`, `format!("{}/models", self.base_url)`.

**PROVE IT.** `grep -n "with_base_url(cx.base_url" crates/oag-server/src/state.rs`; `grep -n normalise_base_url crates/oag-server/src/state.rs` — codex is not among the callers.

**FIX.** `.with_base_url(normalise_base_url("codex", &cx.base_url)?)`.

**TEST.** Extend `a_base_url_is_trimmed_or_refused_at_startup` (state.rs tests)
with a codex case, and add a `codex.rs` unit test that a base URL with a
trailing slash still produces a single-slash `/responses` path (the adapter
should also be defensive: `trim_end_matches('/')` at `with_base_url`).
Partial-revert: remove the `normalise_base_url` call for codex only.

**VERIFY.** `cargo test -p oag-server base_url`, `cargo test -p oag-upstream codex`.

---

### C4 — 0016 dropped an index on a false premise (S6)

**WRONG.** `migrations/0016` says "the query has no `schedulable` predicate at
all". `repo::route_channels` has `AND a.schedulable`. The drop is probably
still safe — neither query leads with `provider`, so the partial index was
unusable by both — but the stated reason is false and there is no test.

**WHERE.** `migrations/0016_drop_unused_account_index.sql` lines 14–24;
`crates/oag-store/src/repo.rs` anchor `AND a.schedulable` in `route_channels`.

**PROVE IT.** `grep -n "AND a.schedulable" crates/oag-store/src/repo.rs`; `sed -n 14,24p migrations/0016_drop_unused_account_index.sql`.

**FIX.** Migrations already applied anywhere must not be edited — check: this
one has reached **no** live database (Groups 0/2/4 are unmerged), so the
comment *can* be corrected in place. Rewrite the premise: name
`route_channels` as the query that does filter `schedulable`, and state the
real reason the index is unusable (no leading `provider` equality in either
query; the planner cannot use a partial index whose predicate the query does
not imply — or does it? **Check with EXPLAIN** on oag_g0 before the index is
dropped there: `EXPLAIN` `route_channels`'s SQL with the index present, and
confirm no `Index Scan using account_schedulable_idx`). Write what EXPLAIN
showed into the comment.

**TEST.** Gated, in `repo.rs`: `EXPLAIN (FORMAT JSON)` `route_channels`'s
statement on a seeded database and assert no node references
`account_schedulable_idx` — this is the same shape as the S1 EXPLAIN test.
Note the index is already dropped by 0016 on a migrated test DB, so the test
asserts the *plan is unchanged by its absence*: it must pass after 0016, and
its value is pinning the query shape so a future `WHERE provider = $1` on this
query re-opens the question with a failing test. Comment that.

**VERIFY.** gated store run.

---

### C5 — `builder_client` promises no panic and panics (U7)

**WRONG.** `reqwest::Client::builder().build().unwrap_or_default()` —
`Client::default()` calls `Client::new()`, which `expect`s. The comment says
the fallback avoids a panic and defers the failure to first use; first use is
the request path.

**WHERE.** `crates/oag-upstream/src/lib.rs` — anchor `pub(crate) fn builder_client`, `.build().unwrap_or_default()`.

**PROVE IT.** Read reqwest's `impl Default for Client` in
`~/.cargo/registry/src/*/reqwest-0.12*/src/async_impl/client.rs` — it is
`Self::new()`, which panics on builder error.

**FIX.** Two honest options; take the first unless it fans out too far:
1. `builder_client() -> Result<reqwest::Client, Error>` and `?` it through
   each adapter's `build()` so a TLS-init failure is `Error::Config` at
   startup (the adapters are built in `state.rs` at boot — that is where a
   config error belongs).
2. Keep the signature; rewrite the comment to say plainly that a TLS-init
   failure still panics on first use and why that is tolerated.
Option 1 is the right one; the second only exists if option 1 touches more
than ~10 sites.

**TEST.** If option 1: a unit test that a builder forced to fail (e.g. an
invalid proxy URL via `ClientBuilder::proxy(Proxy::all("::bad")?)` — find a
builder input that reliably errors) yields `Err`, not a panic. Partial-revert:
put `unwrap_or_default` back.

**VERIFY.** `cargo test -p oag-upstream builder_client`, `just check`.

---

### C6 — streamed refusal rarely maps to `Refusal` (H2, streamed half)

**WRONG.** `parse_event` sets `StopReason::Refusal` only when the refusal
delta is in the same chunk as `finish_reason`. OpenAI sends `finish_reason` in
its own `delta: {}` chunk, so in practice a streamed refusal stops as
`end_turn`. The text half is fixed (no `EmptyResponse` escalation); the stop
reason is not. The test asserts `!= EmptyResponse` — written around the gap.

**WHERE.** `crates/oag-proto/src/openai.rs` — anchor `// \`refused\` only covers a refusal arriving in this same chunk`; `fn a_streamed_refusal_is_read_as_text_too`.

**PROVE IT.** Write the test below first: feed two chunks — one with
`delta.refusal = "no"`, the next with `finish_reason = "stop"` and an empty
delta — through the stream accumulator and assert the final stop reason is
`Refusal`. It fails.

**FIX.** The accumulator (`stream.rs`) already distinguishes a refusal delta
from a text delta (see the comment at that anchor). Track `saw_refusal` on the
accumulator state, and in the `finish_reason` arm, if `saw_refusal` → `Refusal`
regardless of chunk boundary. Rewrite the comment: same-chunk is the *rare*
case.

**TEST.** As above, plus the existing test tightened to `== Refusal`.
Partial-revert: drop the `saw_refusal` consult in the finish arm.

**VERIFY.** `cargo test -p oag-proto refusal`.

---

### C7 — 0015's comment overreaches; read paths still cast to `(14,6)`

**WRONG.** 0015 says "nothing rounds on the way out". `principal_usage` and
`key_usage` cast every ledger sum to `::numeric(14,6)` while returning the
widened `(16,8)` counter beside it; a panel can show a counter disagreeing
with its own ledger sum in the last two digits.

**WHERE.** `crates/oag-store/src/repo.rs` — every `::numeric(14,6)` (anchors
listed by `grep -n "numeric(14,6)" crates/oag-store/src/repo.rs`; ~12 sites in
`principal_usage`, `origin_breakdown`, `key_usage`). `migrations/0015` comment
"Nothing rounds on the way out".

**PROVE IT.** The grep above; then `sed -n 25,32p migrations/0015_widen_spend_counters.sql`.

**FIX.** Widen every output cast to `::numeric(16,8)`. Check the Rust side:
the row structs decode into `Decimal`, which is scale-agnostic, so no type
change; but check any JSON rendering that formats to 6 places and any hurl
assertion on those fields. Leave 0015's comment as is once it is true.

**TEST.** Gated: insert a ledger row with `cost_usd = 0.00000001` and a
matching counter debit; assert `key_usage`'s `month_usd` equals the counter
exactly (`assert_eq!` on `Decimal`). Fails at `(14,6)`, passes at `(16,8)`.

**VERIFY.** gated store run; `./deploy/test/api/api` hurl files if they assert
scale (grep `usd` in `deploy/test/api/*.hurl`).

---

### C8 — a `translate-verify.sh` assertion that cannot fail

**WRONG.** Stage 5's `grep -q 'POST /v1/messages' "$WORK/mock.log"` — the log
is cumulative and stage 4 already required that line. Stage 6 does it right
with a `wc -l` mark.

**WHERE.** `deploy/test/translate-verify.sh` — the second `grep -q 'POST /v1/messages'`; the pattern to copy is `before="$(wc -l <"$WORK/mock.log")"` in stage 6.

**PROVE IT.** Comment out stage 5's curl; run the script; stage 5's grep still passes.

**FIX.** Take `before="$(wc -l <"$WORK/mock.log")"` before stage 5's curl, then
assert `tail -n +$((before + 1)) "$WORK/mock.log" | grep -q 'POST /v1/messages'`.

**TEST/VERIFY.** With the curl commented out, the stage now fails; restore, passes.

---

### C9 — `MOCK_FAIL_FOR_KEY` never matches a bearer credential

**WRONG.** `raw = headers.get("x-api-key") or headers.get("authorization")`
returns `"Bearer <token>"` for bearer providers, so a `<cred>=status` pair never
matches them. Works today only because Anthropic sends `x-api-key`.

**WHERE.** `deploy/test/mock-upstream.py` — anchor `raw = headers.get("x-api-key") or headers.get("authorization") or ""`.

**FIX.** Strip a leading `Bearer ` (case-insensitive) before matching and
digesting. Update the docstring.

**TEST.** A `python3 -m unittest`-free check is fine here: in
`breaker-verify.sh`'s failover stage nothing changes (Anthropic). Add a
5-line self-test at the bottom of `mock-upstream.py` under
`if __name__ == "__main__" and os.environ.get("MOCK_SELFTEST")` that asserts
`_credential({"authorization": "Bearer abc"}) == "abc"`. Run it in
`deploy-checks` CI beside `tofu-verify.sh`.

---

### P2 — the test gaps

Take the **eleven revert-passing tests first**; they are actively misleading.
For each: make it exercise the *wiring* (call the public entry point, not the
helper), then do the partial-revert check.

| test | file | what to change |
|---|---|---|
| `only_a_climb_that_budget_alone_prevented_is_a_suppression` | `oag-server/src/gateway/mod.rs` | must observe `oag_escalations_suppressed_total` via the metrics recorder, not `should_climb`; partial-revert target is the `if gate.is_some() && pressure != Normal` at the counter |
| `the_usage_panels_bound_the_ledger_side_of_their_joins` | `oag-store/src/repo.rs` | `EXPLAIN` the real `key_usage` statement — extract it to a `const KEY_USAGE_SQL` like `ORIGIN_BREAKDOWN_SQL` — and add `principal_usage` |
| `reconcile_does_not_lose_a_debit_that_lands_while_it_runs` | `oag-store/src/repo.rs` | blocked-probe must filter `pid <> pg_backend_pid()` AND match the reconcile's query text, so a sibling's `db.migrate()` advisory lock cannot satisfy it |
| G2 `fold_payloads` tests | `oag-upstream/src/sse.rs` | drive `pump` (harness exists at the bottom of the file) and assert: ledger reason is the provider's, and no second frame is emitted |
| G3 `recorded_gate` test | `oag-server/src/gateway/meter.rs` | go through `stream_response` / `run_with_escalation` with the `state()` harness; partial-revert target is the `triggering_gate` argument at the call site |
| `lowering_a_budget_at_the_cli_evicts_the_cached_identities` | `oag/src/admin/mod.rs` | call `init`, not `evict_principal_keys` |
| `an_admin_key_is_refused_for_a_principal_who_is_not_one` | `oag/src/admin/mod.rs` | call `mint_key` (after B5 moves the check there) |
| `a_gap_or_a_failed_migration_is_not_a_healthy_schema` | `oag/src/admin/doctor.rs` | add the `success = false` case |
| `a_nonsense_wait_from_redis_is_no_wait_rather_than_a_panic` | `oag-store/src/cache.rs` | call `take_rate_token` against a Redis primed with a NaN/negative wait via a stub script, not a reimplemented closure |
| `an_exception_body_that_is_not_utf8_still_becomes_an_error` | `oag-upstream/src/eventstream.rs` | feed a valid multibyte sequence (`C3 A9` = `é`) and assert it appears **verbatim** in the message; Latin-1 decoding would yield `Ã©` |
| `a_streamed_refusal_is_read_as_text_too` | `oag-proto/src/openai.rs` | covered by C6 |

Then the **no-test-at-all** list, with judgement: H4 (done in B7), `64fc95b`'s
`selection_reason` filter (one gated test on `seat_summaries` with an abandoned
row — likely the same test as B7's), A4 (echo the effective budget — one
handler test), C8's `add_account_from_args` exclusion, C9's CLI branch, U2 (10 s
timeout — one test with a never-answering listener), G8's `AppState` refusal,
S6 (C4). **Skip** U8, U11, A5/A6, A8, A11 — comment and metric-description
corrections where a test would be theatre — and say so in the PR body.

**VERIFY.** For every test touched: total revert fails, partial revert fails.
List both outcomes per test in the commit body. "11 for 11, both ways."

---

### P3 — false comments, one commit per PR's worth

Mechanical. Fix; do not re-argue.

**Displaced or doubled doc blocks** — the pattern is text inserted between a
`///` block and its item, or a `///` line pasted onto the end of another:
- `crates/oag/src/admin/mod.rs` — the 20-line block starting `/// Create a principal, or update the budget of one that exists.` is attached to `require_admin_principal`; move it back to `upsert_principal`, and restore `principal_role`'s doc. Three doubled lines: grep `/// .*/// ` in this file and `usage_import.rs`.
- `crates/oag-server/src/gateway/mod.rs` — two doubled lines (`R1, the wiring`, `G4. Budget pressure`); grep `/// .*/// `.
- `crates/oag-router/src/policy.rs` — R1's doc block sits on R10's test; the R1 test below it has none.
- `crates/oag-server/src/health.rs` — `cached_readiness` was inserted between `ready`'s doc and `ready`; move the doc back onto `ready`, give `cached_readiness` its own (and delete its false claim about drain).
- `crates/oag-core/src/config.rs` — the `failover_budget` paragraph is attached to the `usage_poll_interval` `if`; move it to the `failover_budget` check. Delete "`oag serve` still honours zero" and "or unset every `usage_reserve_pct` first" — the refusal is unconditional.

**Claims the code does not support:**
- `crates/oag-proto/src/openai.rs` — "the `tool` branch above" → "below".
- `deploy/tofu/modules/data-neutral/main.tf` — error text "looks like an Upstash URL" → "is not `rediss://`".
- `deploy/tofu/modules/data-azure/main.tf` — two references to `docs/01-deployment.md` explaining `redis_private`: either add the paragraph to the doc or delete the references. Add the paragraph.
- `deploy/helm/open-ai-gateway/templates/prometheusrule.yaml` — "holds the same rules… two things differ": list what actually differs (names, windows, three rules absent, two new), or make them the same. Make the comment true; do not restructure.
- `deploy/helm/open-ai-gateway/values.yaml` — "fourteen" → count them.
- `crates/oag-server/src/gateway/mod.rs` `spawn_unserved` doc — "has already handed" → "will hand".
- `crates/oag-store/src/repo.rs` `budgeted_principal` fixture doc — "below" → "above".
- `deploy/test/kind-verify.sh` — delete the now-unreachable second `[ -n "$PG" ] || fail` and its comment.
- `crates/oag-upstream/src/select.rs` — test comment says "not its default" while comparing against the default; fix whichever is wrong.
- `crates/oag-upstream/src/codex.rs` — two unrelated paragraphs run together; separate them.

**Records:**
- `docs/research/review-full-2026-09-05-consolidated.md` line ~99 — test name → `the_session_timezone_is_utc_because_the_client_asked`.
- `.claude/commands/remediate.md` Group 4 row — "A5–A11" → "A5, A6, A8–A11".

**VERIFY.** `cargo doc --workspace --no-deps 2>&1 | grep -i warn` shows nothing new; `just check`.

---

### P4 — bookkeeping

- **D13 tag.** `git tag -l v0.1.0` is empty. Publishing a tag is the owner's
  call (it triggers the image build workflow). Do not create it; list it in
  the PR body as the one item requiring the owner, with the exact command:
  `git tag -a v0.1.0 -m "…" <sha> && git push origin v0.1.0`.
- **C17.** Read its text in `review-full-2026-09-05.md`. If S2's CLI change
  closes it, add it to Group 2's row in `remediate.md` and to the verdict
  table; if not, it is an open finding — say so.
- **`breaker-verify.sh`** `credentials()` — `grep -c . || echo 0` → `grep -c . || true`.
- **`compute-cloudrun/variables.tf` `deletion_protection`** — no stack passes
  it. Pass it from `stacks/gcp-cloudrun` (default false) so the variable is
  reachable, or delete it. Pass it.
- **`side_channel_client`** is `pub` with only crate-internal callers → `pub(crate)`.

---

### Stage 2 close-out

1. Full gate (§0), all five verify scripts, `tofu-verify.sh`, `helm-render-verify.sh`.
2. Push. Open PR #72: base `review/group-6-tests-docs`, title
   `review fixes: partial fixes, test gaps, and false comments`, body in the
   shape of `group-6-pr.md`: a table of C/P items by finding, the D8 decision
   and its upgrade test, the revert-check tally both ways, the two things left
   for the owner (the tag; any C17 verdict).
3. Report, then stop.

---

## Appendix — the reviewers' claims I did NOT verify by hand

Everything marked in the findings document without **[verified]**. Confirm
each with its PROVE IT before changing code. If a PROVE IT does not reproduce,
the item is a refutation: record it in the PR body with the evidence and move
on. Do not fix what you could not see.
