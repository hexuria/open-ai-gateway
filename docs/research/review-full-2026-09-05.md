# Full-repository review — 2026-09-05

Scope: the whole repository at `af0b0ea` (`main`). Nine review lanes, each briefed
with an evidence bar requiring a location, a defect statement, a concrete failure,
a severity and a confidence. 126 findings survived that bar.

**13 high · 54 medium · 59 low.**

Every high-severity finding below was re-verified by hand against the code before
being written here; three of them were introduced by this session's own work and
are marked as such.

## 1. Scope and coverage

| Lane | Reviewed | Prefix |
|---|---|---|
| Proto | `crates/oag-proto/` — the 4×4 dialect matrix | `P` |
| Gateway | `crates/oag-server/src/gateway/` — request path, streaming, metering | `G` |
| Store | `crates/oag-store/`, `migrations/` | `S` |
| Pure | `crates/oag-core/`, `oag-router/`, `oag-pool/` | `R` |
| Upstream | `crates/oag-upstream/` — adapters, signing, transport | `U` |
| Server | `crates/oag-server/src/` outside `gateway/` — admin, listeners | `A` |
| CLI | `crates/oag/src/` — the binary and `oag admin` | `C` |
| Deploy | `deploy/` except `deploy/test/`, `.github/`, `justfile`, `Cargo.toml` | `D` |
| Docs & tests | `docs/`, `config.example.yaml`, `ERRORS.md`, `deploy/test/` | `X` |

**State of the tree.** The gate is green and was run once before the lanes started:
`cargo fmt --check`, pedantic clippy at `-D warnings`, 733 workspace tests, 40
Postgres/Redis-gated store tests, all four `deploy/test/*-verify.sh` scripts, the
Helm render check, and `cargo audit`. Nothing below is caught by any of them,
which is the point: the defects are at seams the gate does not cross.

**Not covered.** Individual `deploy/test/api/*.hurl` assertion bodies; Grafana
panel structure beyond the PromQL; `deploy/codex-instructions.txt`; the SSRF
denylist in `oag-core/src/service.rs` against non-AWS metadata endpoints; and any
claim requiring a live cloud apply or a ledger large enough to change a query
plan. Each lane names its own gap at the end of its section.

## 2. Findings that block a merge

### H1 — An array-form system message is dropped, so the model gets no instructions

**high** / verified. `crates/oag-proto/src/openai.rs:332` — `parse_one_message`.

The `system`/`developer` branch reads `m["content"].as_str()` and returns; a
system message whose `content` is the legal array-of-parts form is discarded
entirely. The user-content branch forty lines below handles arrays correctly, as
does the `tool` branch, so this is a gap in one arm rather than a dialect limit.

*Failure.* `POST /v1/chat/completions` with
`{"messages":[{"role":"system","content":[{"type":"text","text":"You are a SQL generator. Never write prose."}]},…]}`.
`CanonicalRequest.system` ends up empty, so the Anthropic renderer emits no
`system` and the Responses renderer no `instructions`. The upstream is asked the
user's question with no instructions at all, and nothing in the response says a
field was dropped.

*Fix.* Accept `Value::Array(parts)` in that branch, pushing a `ContentBlock::Text`
per `p["text"]`, mirroring the user-content match.

### H2 — A Chat Completions refusal reads as an empty answer, escalating and double-billing

**high** / verified. `crates/oag-proto/src/openai.rs:576` (`parse_response`) and
`:471` (`parse_event`).

Neither reader looks at `refusal`, which is where this dialect puts a refusal's
text with `content: null`. The only mention of the word in the file is at line
837, in the render direction. `responses.rs` reads refusals in both of its
readers, so this is the one cell of the matrix that does not.

*Failure.* An upstream returns
`{"choices":[{"message":{"content":null,"refusal":"I'm sorry, I can't help with that."},"finish_reason":"stop"}]}`.
The accumulator sees no text and no tool calls, so `quality_gate()` returns
`EmptyResponse`, the gateway escalates one rung and pays a more expensive model
to refuse again. The client then receives `"content": ""`.

*Fix.* Treat `message["refusal"]` / `delta["refusal"]` as a `TextDelta` and map
the stop to `StopReason::Refusal`, as `responses.rs` already does.

### H3 — `reasoning_effort` never reaches an Anthropic or Gemini upstream

**high** / verified. `crates/oag-proto/src/anthropic.rs:43` and
`crates/oag-proto/src/gemini.rs:72` — `render_request`.

Both gate on `req.thinking_budget` alone. The OpenAI renderer does the reverse
bridge at `openai.rs:63` (`thinking_effort.or_else(|| thinking_budget.map(Effort::from_budget))`),
and `openai::parse_request` deliberately sets `thinking_budget: None`, so a Chat
Completions client's `reasoning_effort` has nowhere to land.

*Failure.* `{"model":"oag/auto","reasoning_effort":"high"}` routed to a Claude or
Gemini model emits no `thinking` block and no `thinkingConfig`. The client asked
for maximum reasoning, paid frontier prices, and got a non-thinking answer.
Finding P4 compounds it: `signal()` also ignores `thinking_effort`, so the
classifier does not even route the request as hard.

*Fix.* `req.thinking_budget.or_else(|| req.thinking_effort.map(Effort::as_budget))`
in both renderers, and add the effort clause to `signal()`.

### H4 — Three ledger writes are awaited inline, so a client hang-up cancels them

**high** / verified. `crates/oag-server/src/gateway/mod.rs:1167`, `:354`, `:462`.
**Introduced in this session.**

The served-row write at `mod.rs:496` is deliberately `tokio::spawn`ed with the
shutdown guard, and its comment says why: an inline `.await` there went with the
cancelled handler future, leaving no row and no debit. The three writes for
attempts the provider *did* generate and invoice are still inline `.await`s on
that same cancellable future.

*Failure.* A non-streaming request to a Codex seat generates 500 output tokens and
loses the stream. The client has already timed out and closed the socket, hyper
drops the handler future, the future is cancelled inside `record_usage`, and the
INSERT never commits. The provider invoices the tokens; the ledger has no row.
For the two `record_abandoned` calls, this is the last chance those rows have, as
their own comments state.

*Fix.* `tokio::spawn` all three with a cloned `Arc<AppState>` and a
`lifecycle.track()` guard, the way `mod.rs:495` already does.

### H5 — A lost stream's ledger row displaces the served row that follows it

**high** / verified against the live schema. `crates/oag-server/src/gateway/mod.rs:1167`
with `crates/oag-store/src/repo.rs` (`ON CONFLICT DO NOTHING`).
**Introduced in this session.**

`usage_event`'s primary key is still `PRIMARY KEY (request_id)` — confirmed by
querying the dev database — because migration 0003 deliberately did not contract
it. `record_abandoned` is therefore ordered *after* the served row so the served
row wins. `record_lost` is awaited *before* the retry, so the lost row lands
first and the served row is the one silently dropped.

*Failure.* A stream dies on credential A; credential B serves the retry
successfully. The ledger keeps one row: `selection_reason = lost`, `status = 502`,
`counterfactual_usd = 0`, carrying only A's partial cost. B's full generation is
never billed to the key, the principal or the route, because the debit CTEs run
off the inserted row. A request the client saw succeed appears in the dashboard
as a failed one with zero savings.

*Fix.* Hold the lost attempt the way `abandoned` is held and write it after the
served row, or bring forward the migration that contracts the key onto
`(request_id, attempt)`. Note this is a regression: before the `record_lost`
commit the lost attempt was simply unmetered and the served row was correct.

### H6 — The spend reconcile never runs in the default deployment

**high** / verified. `crates/oag-server/src/lib.rs:383` versus `:400`.
**Introduced in this session.**

`spawn_spend_reconcile` is called only inside the `single_listener` branch. The
default two-listener path spawns `spawn_catalog_refresh` and `spawn_usage_poll`
and not the reconcile, so `gateway.spend_reconcile_interval` is a config field
nothing reads in every documented deployment except Cloud Run and Container Apps.

*Failure.* `oag migrate` runs one reconcile pass at deploy time and then nothing
runs it again for the rest of the month. The rolling-deploy window the pass exists
to close reopens: a principal that spent $400 during the window keeps a counter
reading $0, and its $500 cap admits another $500 of traffic. The Helm chart
renders `spendReconcileIntervalSeconds` into a ConfigMap key that is parsed and
discarded.

*Fix.* Hoist all three spawns above the `if state.config.server.single_listener`
branch so the two paths cannot diverge again.

### H7 — A Bedrock exception frame is decoded and then silently discarded

**high** / verified. `crates/oag-upstream/src/eventstream.rs:189` (`inner_event`)
and `crates/oag-upstream/src/bedrock.rs:184` (`parse_event`).

`inner_event` returns an exception frame's body as a bare string; `parse_event`
hands it to `anthropic::parse_event`, whose dispatch is on `v["type"]`. An AWS
exception payload is `{"message":"…"}` with no `type`, so it falls to the
`_ => vec![]` arm and is erased. The doc comment at `eventstream.rs:186` claims
the opposite: "so the caller can surface the message rather than a silent stall".

*Failure.* A Bedrock stream emits content deltas then a `throttlingException`
frame and ends. The client receives HTTP 200 with a half-finished answer and no
error. The credential is never cooled down, the breaker records nothing, and the
ledger charges for the partial generation. Same for `modelStreamErrorException`
and `validationException`.

*Fix.* Re-wrap an exception frame as `{"type":"error","error":{"message":…}}` in
`eventstream.rs`, where `exception_type` is already in hand, so the existing
`"error"` arm fires.

### H8 — `oag config` prints the database password while its help says secrets are redacted

**high** / verified. `crates/oag-core/src/config.rs:117` and `:147`, with
`crates/oag/src/main.rs:35`.

Only `SecurityConfig` has a hand-written redacting `Debug` (line 174).
`DatabaseConfig` and `RedisConfig` derive it, so `println!("{config:#?}")` prints
the URLs verbatim. The subcommand's help text reads "Show the resolved
configuration, with secrets redacted."

*Failure.* With `OAG_DATABASE__URL=postgres://oag:S3cret@db.internal/oag`,
`oag config` writes the password to stdout. The command exists to be run and
pasted, so the credential lands in support tickets, CI logs and scrollback, and
the operator was told it would not.

*Fix.* Hand-written `Debug` for both config structs replacing userinfo with
`<redacted>`, beside `SecurityConfig`'s.

### H9 — `oag admin key create` prints a key it never stored

**high** / verified. `crates/oag/src/admin/mod.rs:1138` — `mint_key`.

The `INSERT … SELECT … FROM principal p, route r WHERE p.email = $5 AND r.name = $6`
inserts zero rows when either side is missing. The result of `.execute()` is
discarded and the plaintext is returned regardless. The HTTP twin already has the
fix: `repo::mint_key` returns `Option` and the handler answers "no principal with
that email, or no route with that name".

*Failure.* `oag admin key create --email dev@corp.com --route staging` against a
route named `stage` prints the key and "This is shown once". The developer gets
401 on every request, `oag admin key list` shows nothing, and the plaintext is
unrecoverable. The incident reads as broken auth rather than a typo.

*Fix.* `RETURNING id` with `fetch_optional`, and an error naming both lookups.

### H10 — Envoy health-checks a route that does not exist on the port it checks

**high** / verified. `deploy/envoy/envoy.yaml:97` against
`crates/oag-server/src/lib.rs:204` and `:263`.

The active health check GETs `/health/ready` on port 8080. In the two-listener
mode the compose stack runs, `/health/ready` is registered only on the admin
listener; port 8080 carries inference plus `/health/live`.

*Failure.* All three endpoints answer 404, `expected_statuses` defaults to 200, so
every host is marked unhealthy. Envoy's 50% panic threshold then balances to all
hosts regardless of health, which is why the stack appears to work. Both
advertised properties are dead: a replica with a dead pool is never ejected, and
a draining replica is never removed during `just stack-roll`.

*Fix.* Point the check at `/health/live` on 8080, or give each endpoint a
`health_check_config: { port_value: 8081 }` and keep `/health/ready`.

### H11 — Rotating an AWS secret produces a green apply that changes nothing running

**high** / verified. `deploy/tofu/stacks/aws-fargate/main.tf:188` and
`modules/compute-fargate/main.tf:108`.

`secret_env` passes the bare secret ARN with no version stage or id, so a new
`aws_secretsmanager_secret_version` leaves the task definition JSON identical: no
new revision, no ECS deployment. The Cloud Run module documents having fixed
exactly this by pinning the version.

*Failure.* `terraform apply -var credential_kek=<new>` reports one changed secret
version and no ECS change. Every running task keeps the old KEK. Weeks later an
unrelated image bump rolls the tasks, the new ones cannot decrypt credentials
sealed under the old KEK, and every upstream call fails with no deploy in the
change log that touched credentials.

*Fix.* Pass `"${arn}:::${version_id}"`, mirroring the Cloud Run module.

### H12 — `helm upgrade` that changes a secret rolls no pod

**high** / verified. `deploy/helm/open-ai-gateway/templates/deployment.yaml:27`.

The pod template carries `checksum/config` for the ConfigMap and no
`checksum/secret`, so changing `signingSecret`, `credentialKek`, `databaseUrl` or
`redisUrl` updates the Secret and leaves the pod template byte-identical.

*Failure.* A KEK rotation via `helm upgrade` succeeds with nothing to roll. The
rotation then lands pod by pod at arbitrary times through HPA scale-ups and node
drains, giving a fleet where some pods can decrypt sealed credentials and some
cannot, with no deploy to correlate against.

*Fix.* Add a `checksum/secret` annotation, guarded so it is skipped when both
`existingSecret` values are set.

### H13 — `local-verify.sh` asserts on whichever ledger row is newest

**high** / verified. `deploy/test/local-verify.sh:93` with `justfile:308`.
**Introduced in this session.**

PR #64 gave the other three verify scripts a `since` mark taken before each
request. `local-verify.sh` got only the retry loop. Its query has no lower bound
on `occurred_at`, and `coalesce(ttft_ms, 0)` guarantees seven fields, so the
loop's `-ge 5` pipe-count condition is satisfied immediately by any stale row.

*Failure.* `just dev` keeps the Postgres volume. Run `just verify` once, then break
metering entirely: the next run reads the previous run's row, validates it, and
prints "PASS: the request path works end to end".

*Fix.* Add a `mark()` before the request and a `since` argument to
`_verify-ledger`, exactly as the other three scripts now do.

## 3. Medium findings

Grouped by lane. Each carries its location, the defect, the failure, and the fix.

### Proto

- **P4 — `signal()` ignores `thinking_effort`** (`canonical.rs:346`). A Chat
  Completions client's `reasoning_effort: "high"` leaves `thinking_requested`
  false, so the classifier routes a hard reasoning request to the cheap rung.
  Add the effort clause. Compounds H3.
- **P5 — an Anthropic client never learns its prompt token count over a non-Anthropic
  upstream** (`anthropic.rs:624`). `message_delta` carries `output_tokens` only,
  and `message_start` fires before a Chat Completions upstream has reported any
  usage, so `input_tokens` and the cache counts are permanently zero for the
  client. The other three renderers put full merged usage on their terminal frame.
- **P6 — reasoning is dropped by every collected converter** (`openai.rs:818`,
  `gemini.rs:592`, `responses.rs:1070`). All three match only `text` and
  `tool_use`, so `"stream": false` loses the thinking block that `"stream": true`
  delivers. The tokens are billed either way.
- **P7 — the streamed-tool-call stop-reason fix never reached Chat Completions**
  (`openai.rs:527`, `:615`). `responses.rs` and `gemini.rs` consult the
  accumulator; `openai.rs` maps the wire word alone, so an upstream that finishes
  a tool-calling turn with `"stop"` tells an Anthropic client `end_turn` and the
  agent loop never runs the tool.
- **P8 — an empty tool-argument buffer is judged malformed** (`stream.rs:255`).
  A zero-parameter tool call with `arguments: ""` fails `from_str`, so
  `quality_gate` returns `MalformedToolCall` and the gateway pays for a second
  attempt on a valid call. Treat an empty buffer as `{}`.
- **P9 — the Gemini renderer's totals do not add up** (`gemini.rs:627`).
  `promptTokenCount` omits cache-write tokens while `totalTokenCount` includes
  them, so prompt + candidates ≠ total whenever a cache write occurred. The other
  dialects fold writes into the prompt count deliberately.
- **P10 — a tool result reaches Gemini addressed by an opaque call id**
  (`gemini.rs:131`). `functionResponse.name` must be the function's name; for a
  result whose id came from another dialect it is `call_abc123`, so the model
  re-issues the call and the agent loop does not terminate. The name is
  recoverable from the preceding `ToolUse` block.
- **P11 — `max_tokens` is always emitted, which OpenAI's reasoning models reject**
  (`openai.rs:58`). The parser accepts both spellings; the renderer writes only
  the old one, so every request to a gpt-5 or o-series model returns 400.

### Gateway

- **G2 — `pump` mislabels an in-band upstream error as a truncation** (`sse.rs:356`).
  `collect_stream` extracts the provider's message; `pump` does not, so the client
  gets two contradictory error frames and the ledger says the connection truncated
  when the provider said it was overloaded.

### Store

- **S1 — `key_usage` and `principal_usage` aggregate a whole ledger history**
  (`repo.rs:1080`, `:689`). Every window bound is inside `FILTER`, none in
  `WHERE`, so the join reads every row the key ever wrote to report a five-hour
  figure. At ~10M rows the partner service's per-member panel starts timing out.
  This is not covered by the partitioning design doc, which classifies both
  queries as range-bounded — that premise is wrong.
- **S2 — `revoke_key_by_prefix` deactivates every matching key and evicts one**
  (`repo.rs:1152`). `key_prefix` has no unique index and the UPDATE has no LIMIT,
  but the caller takes `fetch_optional`. A prefix collision revokes an unrelated
  customer's key silently, and the intended key may keep authenticating from L2
  for five minutes.
- **S3 — the month boundary is the session timezone in SQL and UTC in Rust**
  (`repo.rs:1021` versus `:813`). Nothing sets `TimeZone` on the pool, so a
  Postgres defaulting to a local zone gives two contradictory month figures in one
  admin response and charges the previous month's spend against this month's cap
  for the offset. Add `set_config('TimeZone','UTC',false)` to `after_connect`.

### Pure crates

- **R1 — four `Error` variants declare a disposition nothing reads** (`error.rs:300`).
  `NoCredential`, `ReserveHeld`, `NoViableModel` and `AtCapacity` are produced only
  by selection, whose errors return to the caller without consulting
  `disposition()`. A route whose only seat is parked at its reserve returns 503
  when a higher rung naming a different provider could have served.
- **R2 — the savings baseline can be cheaper than the model that served**
  (`catalog.rs:208`, used at `policy.rs:354`). `dearest_served` wins whenever the
  served set is non-empty, but that set is a partial discovery: only adapters that
  override `served_models` populate it, and today only Codex does. On a route with
  a Codex seat plus an Anthropic key, a request served by Opus records a
  counterfactual priced at gpt-5 — less than it cost — so the headline
  `SUM(counterfactual - cost)` subtracts on the most expensive rows.
- **R3 — `client_write_timeout` has no ordering constraint against `max_stream_duration`**
  (`config.rs:491`). A write deadline longer than the stream ceiling can never fire,
  and the ceiling is only checked at the top of the pump loop, so a parked send
  holds the slot, the socket and the shutdown guard for the full deadline.
- **R4 — `failover_budget: 0` silently disables failover** (`config.rs:283`). Zero
  means "disabled" for every neighbouring duration; here it means "try exactly one
  credential", voiding `max_account_switches` with no error.
- **R5 — a sticky pin is keyed by route only** (`sticky.rs:117`). A conversation
  crossing rungs overwrites its own pin with the other provider's credential, so
  both providers lose affinity and the prompt-cache hit rate falls with no other
  signal.

### Server and admin

- **A2 — a shed admin auth lookup answers 401** (`admin/auth.rs:57`). Every error,
  including the `Overloaded` the lookup semaphore returns under a key flood, is
  collapsed into "an admin API key is required". The operator opening the
  dashboard to stop the incident is told their key is wrong. The inference path
  preserves `Overloaded` as a 503 with `Retry-After` for exactly this reason.
- **A3 — two summary sections degrade to an empty list indistinguishable from "none"**
  (`admin/mod.rs:350`, `:428`). A timed-out seat query renders the Subscriptions
  section as absent, so an operator concludes the seats were removed. The three
  sibling queries return 500 for the same failure and carry comments saying why.
- **A4 — `POST /admin/api/principals` cannot clear a budget and does not say so**
  (`write.rs:153`). `COALESCE` keeps the existing cap, the comment claims a
  rewrite, and the sibling `PATCH` endpoint treats the identical body as "clear".

### CLI

- **C3 — `account add` reports a credential attached to a route that does not exist**
  (`admin/mod.rs:1497`). The `account_route` insert selects from a missing route and
  inserts nothing, then prints "attached to route 'prod'". The credential is
  schedulable, listed as ready, joined to nothing, and every request fails
  `no_viable_model`.
- **C4 — `oag admin status` counts seat rows in "saved"** (`admin/mod.rs:1731`). The
  admin API filters flat-rate rows out of the headline deliberately; the CLI does
  not, so the two surfaces differ by an order of magnitude.
- **C5 — `oag admin init` changes a budget without evicting cached identities**
  (`admin/mod.rs:1055`). The HTTP path for the same write evicts explicitly; the
  CLI does not, so a lowered cap is unenforced for up to five minutes with no hint
  to flush.
- **C6 — `oag admin init` silently promotes an existing principal to admin**
  (`admin/mod.rs:1061`). Its `ON CONFLICT` sets `role = EXCLUDED.role` with the role
  hard-coded to `admin`; the store's own `upsert_principal` deliberately omits
  `role` from the update and documents why. Adding a route with `init` grants
  admin to whoever the `--email` names and mints them an admin key.
- **C7 — `key create --admin` on a non-admin principal mints an unusable key**
  (`admin/mod.rs:705`). The admin gate needs both the key flag and the principal
  role; nothing checks the role or warns, and the CLI has no command that sets one.
  The troubleshooting doc sends the operator back to the command that just failed.
- **C8 — `OAG_ACCOUNT_SECRET` breaks every `--from` import** (`admin/mod.rs:186`).
  clap treats an env-supplied value as explicitly present for conflict checking, so
  the documented way to keep a key out of shell history disables the seat importers
  with an error naming a flag that is not on the command line.
- **C9 — `key revoke` reports "shared cache evicted" when it was not**
  (`admin/mod.rs:1666`). `auth_invalidate` returns `()` and gives up silently when
  Redis is unreachable. During a leaked-key incident the operator is told the
  shared cache is clear and the residue expires in 15 seconds; it is five minutes.
- **C10 — `doctor` counts an owner-bound seat as a live credential for a rung**
  (`doctor.rs:163`, `:244`). The scheduler filters on `owner_principal_id`; doctor
  and `account list` never select it, so a personally bound seat reports `ok` for
  every principal and no CLI output ever mentions the binding.
- **C11 — a transcript line with no `sessionId` can be imported twice**
  (`usage_import.rs:390`). The idempotency key falls back to the filename, so the
  same message id in a resumed session's file derives two `source_ref`s and one
  API call's tokens are booked twice. The test that pins this writes `sessionId`
  into every fixture line.

### Deploy

- **D4 — Azure "managed" Redis is internet-reachable** (`modules/data-azure/main.tf:48`).
  `public_network_access_enabled` is left at its default `true` with no private
  endpoint, while the stack header says both stores are reached privately over the
  VNet. Anyone with the access key reads the auth cache from anywhere.
- **D5 — Container Apps pays for a premium profile it never uses**
  (`modules/compute-containerapps/main.tf:30`). The environment gets a
  `Dedicated-D4` profile; the container app never sets `workload_profile_name`, so
  it runs on Consumption. Streams still die at 240s and the guard passes.
- **D6 — rotating a GCP secret destroys the version the previous revision pins**
  (`stacks/gcp-cloudrun/main.tf:72`). The default deletion policy removes the old
  version, so rolling back to the previous Cloud Run revision fails with
  "secret version was destroyed". Set `deletion_policy = "DISABLE"`.
- **D7 — the ECS probes are swapped** (`modules/compute-fargate/main.tf:164`, `:62`).
  The container health check is the dependency-checking one and the ALB check is
  not, the reverse of every other platform. An RDS failover recycles the entire
  fleet while the ALB keeps routing to it.
- **D8 — in-cluster mode deletes the Postgres StatefulSet on every upgrade**
  (`templates/data-incluster.yaml:22`). Helm's default hook deletion policy
  recreates both StatefulSets at the start of each `helm upgrade`, so the fleet
  loses its pool for 20-40 seconds before the new gateway pods exist, failing
  in-flight ledger writes.
- **D9 — the PDB preflight cannot fire in the default configuration**
  (`_helpers.tpl:56`). The guard is nested inside `if not autoscaling.enabled`, and
  autoscaling defaults to true. A node drain then hangs forever, which is one of
  the five guarantees the docs claim the chart gives.
- **D10 — fourteen settings are reachable from the chart and the rest are not**
  (`templates/configmap.yaml:7`). `server.max_body_bytes` has no value, no key and
  no `extraEnv` escape hatch, yet the values file and the shedding runbook both
  tell operators to change it. Same for `failover_budget`, `usage_poll_interval`,
  the whole `gateway.codex` block, and `claude_code_model_aliases`.
- **D11 — the Cloudflare keepalive guard checks a variable wired to nothing**
  (`modules/edge-cloudflare/main.tf:36`). No stack turns
  `stream_keepalive_interval_seconds` into the gateway's env var, so the guarded
  number and the deployed number are independent. An operator who raises the real
  one gets 524s the guard promised to prevent.
- **D12 — `.terraform.lock.hcl` is gitignored** (`.gitignore:13`). Constraints are
  `>= 5.0` with no upper bound, so a fresh clone resolves whichever provider major
  is newest that day and two operators on one commit get different plans.
- **D13 — the chart's default image tag has never been published**
  (`values.yaml:8`, `Chart.yaml:6`). `appVersion` is `0.1.0`; the release workflow
  publishes `main`, a sha tag and semver on `v*` pushes, and the repo has no tags.
  The documented install lands on `ImagePullBackOff`.
- **D14 — a proxied Cloudflare CNAME onto a `run.app` origin returns 404**
  (`stacks/gcp-cloudrun/main.tf:172`). Cloud Run routes by `Host` and there is no
  domain mapping, so the apply succeeds, the output prints the custom hostname, and
  every request to it 404s while the `run.app` URL works.
- **D15 — nothing in CI validates Terraform, and `cargo audit` never runs**
  (`.github/workflows/`). Two gates the repo documents as existing run nowhere. A
  malformed `.tf` merges green; a new advisory lands silently and the ignore entry
  in `.cargo/audit.toml` can never expire.

### Docs and tests

- **X2 — `kind-verify.sh` counts the whole ledger table** (`kind-verify.sh:262`).
  The cluster and its PVC are reused on the re-run after a failure, so the count
  already exceeds the threshold and the disconnect-billing check cannot fail.
- **X3 — `bedrock-verify.sh`'s overlay assertion matches the model name**
  (`bedrock-verify.sh:145`). `grep -q 'bedrock'` matches `"model":"bedrock-mock"`
  in `message_start`, so the decoder ships green with zero delivered text.
- **X4 — `dialects-verify.sh`'s OpenAI stream check passes on `[DONE]` alone**
  (`dialects-verify.sh:167`). The content branch is an OR alternative, and there is
  no ledger assertion after either streamed request.
- **X5 — `ERRORS.md` names the wrong knob for tripping the breaker** (`ERRORS.md:135`).
  It cites `MOCK_FAIL_FIRST`, which returns 529 and fails over; the harness uses
  `MOCK_FAIL_STATUS=408`, and the header explains why 5xx cannot reach the
  threshold.
- **X6 — `ERRORS.md` claims two kinds are asserted live and they are not**
  (`ERRORS.md:91`). The `no_viable_model` case asserts only that a type field
  exists, and no hurl file exercises `UnsupportedAction` at all.
- **X7 — the client docs describe an `abandoned` row this release cannot write**
  (`docs/08-clients.md:292`). With the primary key still on `request_id`, the
  abandoned row is dropped, as the store's own tests state.
- **X8 — `helm-render-verify.sh` claims a KEK length check the chart does not have**
  (`helm-render-verify.sh:27`). The only length check is at process start, so a
  48-byte KEK renders fine and every replica crash-loops.
- **X9 — end-to-end coverage gaps at real seams.** No script exercises
  `/v1/responses`, an Anthropic client over a non-Anthropic upstream, Gemini over
  anything else, failover between two credentials, escalation, budget refusal,
  shedding, a client hanging up mid-stream, or the `x-oag-*` response headers.
  Half the hub's dialect matrix rests on unit tests only.

## 4. Low findings

| id | one line | where |
|---|---|---|
| P12 | a Gemini tool result gains a JSON wrapper on every round trip | `gemini.rs:303` |
| P13 | `content_block_stop` emits `ToolUseEnd` for text blocks too | `anthropic.rs:369` |
| G3 | a streamed request that escalated records no triggering gate | `mod.rs:366` |
| G4 | `oag_escalations_suppressed_total` counts non-budget suppressions | `mod.rs:444` |
| G5 | an invalid `x-oag-tier` discards the body's `oag/<rung>` pin | `mod.rs:748` |
| G6 | `ensure_fresh` picks the refresher by provider, not by adapter | `refresh.rs:48` |
| G7 | the half-open probe covers only the first same-credential retry | `mod.rs:1302` |
| G8 | `SLOT_TTL` is unbound to `max_stream_duration`; its test compares two constants | `select.rs:24` |
| S4 | counters are `numeric(14,6)`, the ledger is `numeric(14,8)` | `0001_baseline.sql:156` |
| S5 | `AuthContext.key_hash` is written to every L2 entry and read by nothing | `rows.rs:141` |
| S6 | `account_schedulable_idx` cannot serve the query both migrations name | `0013:12` |
| S7 | an expired key keeps authenticating for the L2 TTL | `repo.rs:97` |
| S8 | `take_rate_token` can panic on a Redis-supplied value | `cache.rs:242` |
| S9 | the reconcile race test can pass with the fix reverted | `repo.rs:2384` |
| R6 | `Candidate::waiting` is always zero; its test pins unreachable behaviour | `schedule.rs:61` |
| R7 | `Catalog::resolve` allocates a `String` per request | `catalog.rs:294` |
| R8 | the tie-break test detects a missing tie-break about half the time | `catalog.rs:372` |
| R9 | `SessionKey`'s doc claims principal scoping the fallback form lacks | `sticky.rs:96` |
| R10 | `hard_stop_multiple` below 1 inverts degrade-before-deny | `policy.rs:73` |
| R11 | duplicate of G4 from the router side | `policy.rs:231` |
| R12 | `usage_poll_interval: 0` silently disables every quota reserve | `config.rs:291` |
| R13 | `humantime_secs` doc says four fields; it serialises eleven | `config.rs:523` |
| U7 | every adapter builds and drops a `reqwest::Client` per request, via a panicking constructor | `anthropic.rs:39` |
| U8 | `sigv4::Credentials` derives `Debug` over a plaintext AWS secret | `sigv4.rs:27` |
| U9 | the xAI refresh token is posted to an unvalidated discovery endpoint | `xai_oauth.rs:166` |
| U10 | duplicate of G6 from the adapter side | `codex.rs:193` |
| U11 | `unreachable!()` on the request path | `sigv4.rs:185` |
| U12 | `proxy_url` applies to inference only, not refresh, quota or price calls | `openai_oauth.rs:131` |
| U13 | the Anthropic OAuth branch is unreachable and would not work | `anthropic.rs:48` |
| U14 | an exception frame's body is decoded as Latin-1 | `eventstream.rs:202` |
| U15 | a Bedrock signing assertion that cannot fail | `bedrock.rs:439` |
| U2 | `served_models` builds a client with no timeout, stalling the whole poller | `codex.rs:203` |
| U3 | `pricing::fetch` dispatches on provider alone, so a seat token hits the API endpoint | `pricing/mod.rs:37` |
| U4 | the inference adapters do not trim the credential | `anthropic.rs:54` |
| U5 | a configured base URL is concatenated with no normalisation | `anthropic.rs:37` |
| U6 | a Bedrock endpoint override with a path corrupts the `Host` header | `bedrock.rs:66` |
| A5 | `oag_credentials_schedulable` is described and never set | `metrics.rs:99` |
| A6 | three emitted metrics are never described | `metrics.rs:32` |
| A7 | the origin table groups seats by name and mixes two counterfactuals | `admin/mod.rs:406` |
| A8 | `/health/ready` is outside the ceiling and costs a pooled connection | `lib.rs:262` |
| A9 | `the_dashboard_and_metrics_stay_open` passes if the routes disappear | `lib.rs:880` |
| A10 | "the three routes outside the admin layer" is four | `lib.rs:268` |
| A11 | a successful probe whose write fails reports `unknown` | `services.rs:242` |
| C12 | `check_seat_prices` and all four of its tests cannot fail | `doctor.rs:291` |
| C13 | `catalog list --provider X` says the catalog is empty | `admin/mod.rs:961` |
| C14 | duplicate account names are creatable and then unfixable from the CLI | `admin/mod.rs:1452` |
| C15 | the importer's price lookup silently picks one of two ambiguous rows | `usage_import.rs:829` |
| C16 | `doctor` counts migration rows rather than versions and ignores `success` | `doctor.rs:48` |
| C17 | revoking a shared prefix evicts one of the keys it deactivated | `admin/mod.rs:1658` |
| D16 | `data-neutral` only checks Redis TLS for Upstash hostnames | `data-neutral/main.tf:25` |
| D17 | the Fargate grace-period comment invents an ECS cap | `compute-fargate/main.tf:217` |
| D18 | `public_subnet_ids` is not ignored when `internal` is true | `aws-fargate/variables.tf:24` |
| D19 | Cloud Run defaults to half the memory the in-flight ceiling is sized for | `compute-cloudrun/variables.tf:60` |
| D20 | the alert rules cannot be used on Kubernetes and the chart ships none | `prometheus/alerts.yml` |
| D21 | `just floci-up` prints success for a gateway that never became ready | `floci/deploy.sh:113` |
| D22 | ElastiCache has no AUTH token, unlike the GCP module | `data-aws/main.tf:75` |
| D23 | dead Caddy matcher, unreachable edge rate limiting, missing Azure log settings, missing Cloud Run `deletion_protection` | various |
| X11 | `ERRORS.md` says 18 error shapes; there are 20 | `ERRORS.md:140` |
| X12 | `docs/01-deployment.md` lists six migrations; there are thirteen | `01-deployment.md:242` |
| X13 | README's test counts are stale by ~240 | `README.md:100` |
| X14 | the Caddyfile says to publish a port compose already publishes | `Caddyfile:107` |
| X15 | the `stream_idle` reproduction recipe does not reproduce it | `ERRORS.md:110` |
| X16 | `local-verify.sh` proceeds past a dead mock and its guard contradicts its unpack | `local-verify.sh:49` |
| X17 | three credential-free verifications are documented; there are four | `07-running-locally.md:30` |

## 5. Considered and dismissed

The lanes examined and rejected these. Re-raising one needs new evidence.

**Money and concurrency.** `reconcile_monthly_spend` racing a debit (the lock is
taken in its own statement before the sum's snapshot, so a concurrent write either
blocks and is added afterwards or was already committed and included). Deadlock
between `record_usage`'s CTEs (every caller runs identical statement text, so the
order is fixed). `api_key.spent_usd` being excluded from the reconcile (the
previous release already maintained it). Imported rows inflating a cap (their
insert omits `principal_id` and `route_id`). The slot count/trim boundary (the
acquire's sweep is inclusive at `now-ttl` and the count exclusive there; they
agree, and the test plants members on both sides).

**Proto.** `parse_response` emitting `UsageUpdate` and `Stop` with the same usage
(`merge` is a per-field max). `has_images` skipping `system` (no parser can put a
non-text block there). Thinking signatures not surviving the response path (the
round-trip claim is about replayed client blocks, which do survive).

**Gateway and pool.** `Lease::clone` double-releasing (one `Arc`'d guard with a
swap-guarded flag). `abandoned` being dropped on a streaming return (an abandoned
attempt and a streaming attempt cannot co-occur). `is_eligible` duplicated between
`select.rs` and `schedule.rs` (they agree, and the split is justified in the
comment). `every_candidate_is_full` (this is the fixed form of the predicate).

**Upstream.** `uri_encode_path` double encoding (verified against the AWS test
vector; the previously refuted claim does not resurface). Bedrock declaring
event-stream framing for non-streaming `invoke` (the collected path never consults
`framing()`). Transport eviction cutting an in-flight stream (`get` hands out an
`Arc`).

**Server and CLI.** Admin writes that change a cap without evicting (all four
evict). `upsert_principal` promoting to admin over HTTP (refused, and the update
never touches `role`). `mint_key` minting an admin key over HTTP (hardcoded
false). Dashboard XSS (every interpolation is in a double-quoted attribute; the
two `href` sites are server-validated). Metric label cardinality (no label takes a
caller-supplied value).

**Deploy and docs.** Migrate-Job pods joining the Service (named `targetPort`s the
migrate pod does not declare). `_dev-secret` tripping the placeholder check. The
provider base URL env casing. `OagDrainStuck` outliving its series. `config.example.yaml`
drift (every key exists; every omission has a default). `ERRORS.md` completeness in
all five directions (all 20 variants present everywhere). `EXPECTED_MIGRATIONS`
(13 and 13, with a test). The `docs/02-cost-routing.md` thresholds and cache TTLs.
`grep … && fail` under `set -e`.

## 6. What to do first

1. **H5 and H4 together**, in `gateway/mod.rs`. They are the same code and the
   same commit, and H5 is a regression that makes a served answer unbillable.
2. **H6**, one line in `lib.rs`, restoring a shipped feature in the default
   deployment.
3. **H1, H2, H3** in `oag-proto`. Each is a client-visible wrong answer and each
   is a few lines.
4. **H8 and H9** in the CLI. One leaks a password, one hands out a key that does
   not exist.
5. **H10, H11, H12** in deploy. None is triggered by a request; all three are
   triggered by the next operator action.
6. **H7** in the Bedrock decoder, and **H13** in `local-verify.sh`.

The mediums split cleanly: the proto and store ones are correctness, the CLI ones
are operator-facing safety, and the deploy ones are all "the next apply does
something you did not ask for". The lows are mostly comments that no longer
describe the code and tests that cannot fail, which is the class this repo treats
as real and which accumulates fastest.
