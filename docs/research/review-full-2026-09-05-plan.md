# Plan: closing the 2026-09-05 review

Source of truth for what is being fixed: `review-full-2026-09-05-consolidated.md`
(123 confirmed findings). Full failure scenarios and suggested fixes:
`review-full-2026-09-05.md`.

## Rules

- One commit per finding, subject prefixed by area (`proto:`, `gateway:`,
  `store:`, `server:`, `cli:`, `tofu:`, `helm:`, `deploy:`, `test:`, `docs:`).
  Body says what was wrong and why the fix is right. A lying comment fixed
  alongside its code goes in the same commit; on its own it is its own commit.
- Every fix that changes behaviour carries a test that fails with the fix
  reverted. Findings tagged "tests that cannot fail" are fixed by making the
  test fail first.
- The gate before every commit: `just check`, then the gated store tests, then
  the four integration scripts; `helm-render-verify.sh` and
  `terraform validate` on both stacks for anything under `deploy/`.
- One PR per wave, stacked on `main`. Merge each when CI is green before
  starting the next, so a wave never carries another wave's regressions.
- Nothing in the "do not re-raise" list is touched.

## Wave 0 — this session's regressions (first, small, one PR)

All four are in code merged today. H4 and H5 share a fix.

| finding | change | size |
|---|---|---|
| H5, H4 | Hold the lost attempt like `abandoned` is held: capture a `meter::Lost` in `forward_with_failover`, return it up with the `Attempt`, and write it after the served row inside the same spawned task that writes the served row. That single change fixes the ordering (H5) and takes all three writes off the cancellable future (H4). | medium |
| H6 | Hoist the three `spawn_*` calls above the `single_listener` branch in `lib.rs`. Add a test that `serve`'s two paths spawn the same set (a small `spawned_tasks()` seam, or assert on a counter the reconcile increments). | small |
| H13 | Give `_verify-ledger` a `since` argument and take a `mark()` before the request in `local-verify.sh`, matching the other three scripts. Fix X16 in the same file: assert the mock came up, and change the guard to `< 7`. | small |
| X7 | Correct the `abandoned`/`lost` rows in `docs/08-clients.md`: not written until the key contracts onto `(request_id, attempt)`. | small |

Gate plus a new gated store test: a lost attempt followed by a served retry
leaves exactly one row, and it is the served one, until the key contracts.

## Wave 1 — wrong answers to clients (`oag-proto`)

| finding | change | size |
|---|---|---|
| H1 | Accept array-form system content in `parse_one_message`. | small |
| H2 | Read `refusal` in `parse_response` and `parse_event`; map to `StopReason::Refusal`. | small |
| H3, P4 | Bridge `thinking_effort` to a budget in the Anthropic and Gemini renderers; add the effort clause to `signal()`. | small |
| P7 | Consult `acc.saw_tool_call()` in `openai::parse_event` and the parsed calls in `parse_response`, as the other two dialects do. | small |
| P8 | Treat an empty argument buffer as `{}` in `quality_gate`. | small |
| P6 | Emit the thinking block from all three collected converters. | small |
| P5 | Put full merged usage on the Anthropic `message_delta`. | small |
| P9 | Fold cache writes into Gemini's `promptTokenCount`, in both usage renderers. | small |
| P10 | Build an id→name map from the request's `ToolUse` blocks before rendering Gemini tool results. | medium |
| P11 | Emit `max_completion_tokens` alongside `max_tokens` when reasoning is requested, and for `Provider::OpenAI`. | small |
| P12 | Read `functionResponse.response.result` when it is a string; assert the result part in the round-trip test. | small |
| P13 | Emit `ToolUseEnd` from `content_block_stop` only when the closing block is a tool block. | small |

Each with a unit test in the crate. `translate-verify.sh` gains a system-message
array and a `reasoning_effort` request (covers X9's cheapest gap).

## Wave 2 — money and the ledger

| finding | change | size |
|---|---|---|
| R2 | Baseline is the dearer of `dearest_served` and the ladder ceiling. Test with a served set cheaper than the served model. | small |
| S1 | Move the widest window bound into the `ON` clause of both joins. Pin with `EXPLAIN` in a gated test. | small |
| S3 | `set_config('TimeZone','UTC',false)` in `after_connect`; test reads `SHOW TimeZone`. | small |
| S2 | `fetch_all` from `revoke_key_by_prefix`; CLI evicts and prints every row. | small |
| G2 | Extract an in-band `Error` event in `pump` as `collect_stream` does; suppress the duplicate frame. | small |
| C4 | Apply the seat-row filter to `oag admin status`; print a seat line. | small |
| A7 | Group `origin_breakdown` by `a.id`; sum `counterfactual_api_usd`. | small |
| G3 | Thread `triggering_gate` into `stream_response` and `meter::record`. | small |
| G4 | Increment the suppression counter only when budget pressure was the sole blocker. | small |
| S4 | Widen the three counters to `numeric(16,8)`: new expand migration, bump `EXPECTED_MIGRATIONS`, fixture costs with eight decimals in the equality tests. | medium |
| S9, R8 | Make the reconcile race test force an interleaving; make the tie-break test use several independently seeded sets. | small |

## Wave 3 — operator safety (CLI and admin)

| finding | change | size |
|---|---|---|
| H8 | Redacting `Debug` for `DatabaseConfig` and `RedisConfig`. Test that `format!("{config:?}")` contains no userinfo. | small |
| H9 | `RETURNING id` + `fetch_optional` in the CLI `mint_key`, or delete it and call the store's. | small |
| C6, C7 | `init` refuses to change an existing principal's role; add `oag admin principal promote`; `key create --admin` refuses on a non-admin principal and names the command. | medium |
| C5 | `init` evicts through `key_hashes_for_principal`. Thread `redis_url` in. | small |
| C3 | Check `rows_affected` on the `account_route` insert; delete the orphan row on zero. | small |
| C9 | `auth_invalidate` returns `Result`; the CLI prints the failure and the five-minute consequence. | small |
| C8 | Drop `secret` from `from`'s conflict set; enforce exclusion in `add_account_from_args`. | small |
| C10 | Select `owner_principal_id` in doctor and `account list`; name the owner; doctor says when a rung's only credential is owner-bound. | medium |
| C11 | Key `source_ref` on the message id alone for Claude Code; fixture without `sessionId`. | small |
| A2 | Map `Overloaded` to 503 with `Retry-After` and other errors to 500 in `require_admin_layer`. | small |
| A3 | Add a `degraded: Vec<&str>` field to `Summary`; dashboard renders a warning line. | small |
| A4 | Echo the effective budget from the upsert; fix the comment. | small |
| C12–C16 | `check_seat_prices` returns the names; `catalog list` distinguishes empty-after-filter; reject duplicate account names and add `account rename`; `Prices::index` refuses ambiguous keys; `check_migrations` requires versions `1..=N` with `success`. | medium |

## Wave 4 — upstream and the pure crates

| finding | change | size |
|---|---|---|
| H7 | Re-wrap an exception frame as `{"type":"error",…}` in `eventstream.rs`; UTF-8 decode (U14). Test with a `throttlingException` frame through `BedrockAdapter::parse_event`. | small |
| U2 | 10s timeout on the `served_models` client. | small |
| U3 | `CredentialKind` parameter on `pricing::fetch`; `None` for non-API-key kinds. | small |
| U4 | Trim the credential once in `SecretMaterial` on open. | small |
| U5, U6 | Normalise base URLs at adapter construction: trim trailing slash, reject query/fragment, parse the Bedrock host from the URL. Config error at boot. | medium |
| R1 | Delete the three unreachable disposition arms, or consult `disposition()` on the selection error path in `run_with_escalation`. Decide: the delete is honest; the consult is the feature the comment promises. Recommend the consult, with a test that a reserve-held rung escalates. | medium |
| R3, R4 | Validate `client_write_timeout` and `upstream_response_timeout` below `max_stream_duration`; reject `failover_budget: 0`. | small |
| R5 | Namespace the sticky key by provider. | small |
| G6 | `ensure_fresh` uses `adapter_for`. | small |
| G5 | Filter the header before the `or_else` fallback. | small |
| G7 | Re-check `permits` at the top of the retry loop. | small |
| G8 | Validate `max_stream_duration < SLOT_TTL`; rewrite the test against the default config. | small |
| R6, R7, R9, R10, R12, R13 | Drop `waiting`; `Borrow<str>` for `ModelId`; fix the `SessionKey` doc; clamp `hard_stop_multiple`; document zero for `usage_poll_interval`; fix the "four fields" comment. | small each |
| S5–S8 | Delete `key_hash`; drop or retarget `account_schedulable_idx` (migration + bump); cap L2 TTL by `expires_at`; `try_from_secs_f64` in `take_rate_token`. | small each |
| U7–U9, U11–U13, U15 | One lazily built builder client per adapter; redacting `Debug` on `sigv4::Credentials`; validate `token_endpoint` scheme and host; remove `unreachable!()`; route refresh/quota/price calls through the proxy; delete the Anthropic OAuth branch; recompute the signed host in the Bedrock test. | small each |
| A5, A6, A8–A11 | Set or delete `oag_credentials_schedulable`; describe the three counters; memoise readiness for one second; assert `NOT_FOUND` in the open-routes test; correct "three" to four; set `last_ok` in the probe fallback. | small each |

## Wave 5 — deploy

Decisions needed before this wave, in order of consequence:

- **D4** Azure Redis private endpoint requires a Premium SKU or a private
  endpoint on Standard. Cost decision.
- **D22** ElastiCache AUTH token: gated on `tls`, as the GCP module does. Yes
  unless there is a reason not to.
- **D12** Committing lock files and tightening constraints to `~> 7` / `~> 6`.
  Recommended.
- **D13** Either publish a `v0.1.0` tag or default the chart tag to `main`.
  Recommend the tag; it is what the semver publication step is for.

| finding | change | size |
|---|---|---|
| H10 | Envoy: `health_check_config: { port_value: 8081 }` per endpoint, keep `/health/ready`. Fix `docs/01-deployment.md:43`. | small |
| H11 | `"${arn}:::${version_id}"` in the Fargate `secret_env`. | small |
| H12 | `checksum/secret` annotation, skipped when both `existingSecret`s are set. Assert it in `helm-render-verify.sh`. | small |
| D6 | `deletion_policy = "DISABLE"` on the GCP secret versions. | small |
| D7 | Swap the Fargate probes; open 8081 from the LB security group. | small |
| D8 | Drop the hook annotations from the in-cluster StatefulSets; keep ordering via the migrate Job's init container. | small |
| D9 | PDB guard compares against `minReplicas` when autoscaling is on. Add a render check that it fires. | small |
| D10 | `extraEnv` in the chart; `server.maxBodyBytes` value and key. | small |
| D11 | Each stack merges `OAG_GATEWAY__STREAM_KEEPALIVE_INTERVAL` from the guarded variable into the compute env. | small |
| D15 | CI job: `terraform fmt -check`, `init -backend=false && validate` per stack and module; `cargo audit`. | small |
| D5 | `workload_profile_name` on the Container App. | small |
| D14 | `google_cloud_run_domain_mapping` for `hostname`, or a precondition refusing the combination. | small |
| D16–D21, D23 | Unconditional Redis TLS check; delete the invented ECS cap; `internal ? private : public` subnets; Cloud Run memory `1Gi`; `PrometheusRule` template with `pod` labels; success flag in `floci/deploy.sh`; Caddy matcher, edge rate-limit pass-through, Azure `LOG_JSON`, service `deletion_protection = false`. | small each |

`terraform validate` on every stack and module, `helm-render-verify.sh`, and
one `kind-verify.sh` run for D8 and D9.

## Wave 6 — tests and docs

| finding | change | size |
|---|---|---|
| X2 | Count rows newer than a mark in `kind-verify.sh`. | small |
| X3, X4 | Reassemble stream text in the Bedrock and OpenAI stream checks; assert the ledger after streamed requests. | small |
| X9 | Add: a `/v1/responses` request to `translate-verify.sh`; a second account to `breaker-verify.sh` to pin failover; an `x-oag-*` header assertion to `local-verify.sh`. Escalation, budget refusal and shedding are a follow-on script against the existing mock. | medium |
| X5, X6, X11, X15 | `ERRORS.md`: name `MOCK_FAIL_STATUS=408`; assert `no_viable_model` and add a `not_found` request to `gemini.hurl`; say 20; give the `stream_idle` recipe that reaches the watchdog. | small |
| X8, X12, X13, X14, X17 | Correct the KEK comment, regenerate the migrations table, update the README counts, fix the Caddyfile note, list the fourth verification. | small each |
| U15, A9, C12 | Covered in Waves 3–4. | — |

## Order and effort

| wave | commits | PRs | effort |
|---|---|---|---|
| 0 | 5 | 1 | half a day |
| 1 | 12 | 1 | one day |
| 2 | 11 | 1 | one day |
| 3 | 18 | 1 | one and a half days |
| 4 | ~30 | 1–2 | two days |
| 5 | ~22 | 1–2 | one and a half days, after the four decisions |
| 6 | ~12 | 1 | one day |

Waves 0 and 1 are the ones that change what a client receives, and Wave 0 is a
regression of today's work; they go first and separately. Waves 2 and 3 are
independent of each other and of Wave 4. Wave 5 waits on the decisions above
and touches nothing the earlier waves touch. Wave 6 can be split across the
others where a script assertion belongs with the fix it pins.

## After the last wave

Contract the ledger's primary key onto `(request_id, attempt)`: it is the
prerequisite for both the `abandoned` and `lost` rows to survive, it is the
identity table step in the partitioning design, and every wave above leaves
it as the remaining reason two of the ledger's documented rows do not exist.
It is one migration, the removal of two tests that pin the old key, and the
deletion of the "held rather than written" ordering in `run_with_escalation`.
