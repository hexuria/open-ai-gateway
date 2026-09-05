# Consolidated review findings — 2026-09-05

The lane reports in `review-full-2026-09-05.md` were re-verified one by one
against `main` at `af0b0ea`: every cited location was read, and the claimed
failure was traced through the surrounding code. This document is the outcome
and is the source of truth for the plan in `review-full-2026-09-05-plan.md`.

## Result

| | Count |
|---|---|
| Findings reported by the lanes | 126 |
| Merged as duplicates across lanes | 3 |
| Severity changed on re-verification | 2 |
| Refuted | 0 |
| **Distinct confirmed findings** | **123** |

| Severity | Count |
|---|---|
| High | 13 |
| Medium | 53 |
| Low | 57 |

**Merged.** R11 into G4 (the suppressed-escalation counter, seen from the router
and the gateway). U10 into G6 (`ensure_fresh` choosing its refresher by provider,
seen from the adapter and the gateway). C17 into S2 (revoking a shared prefix,
seen from the CLI and the store).

**Severity changed.** X10 promoted from medium to high, as H5: the primary key
was confirmed against the live schema, so the served row really is dropped. X8
downgraded from medium to low: the false claim about KEK validation lives in a
test script's comment, not in operator-facing documentation.

**Confirmed on documented behaviour rather than execution.** Four findings
depend on a platform's behaviour that I verified from its documentation and the
code's own comments, not by running it: C8 (clap treats an env-supplied value
as explicit for conflict checks), D5 (azurerm's `workload_profile_name` default),
D14 (Cloud Run routes by `Host` and needs a domain mapping), U4 (`reqwest`
defers an invalid header value to `build()`). Each is marked in the table.

**Verified by construction.** Three test-integrity findings concern tests I
wrote in this session and are confirmed by the tests' own shape: S9 (the
reconcile race test has no forced interleaving), R8 (the tie-break test iterates
one `HashSet` instance), H13 (`local-verify.sh` got the loop and not the mark).

## Regressions from this session

Four findings are defects in commits made earlier today on this branch. They
are listed first in the plan and should be fixed before anything else.

| id | commit | defect |
|---|---|---|
| H4 | `f4143d2`, `8781acf` | `record_lost` and both `record_abandoned` flushes are awaited on the cancellable request future |
| H5 | `f4143d2` | `record_lost` writes before the retry, so the served row loses the `request_id` conflict |
| H6 | `995bfef` | `spawn_spend_reconcile` is called only on the single-listener path |
| H13 | `68241fd` | `local-verify.sh` received the retry loop and not the `since` mark |

X7 is related: the `abandoned` and `lost` rows I documented in
`docs/08-clients.md` cannot both survive while the primary key is `request_id`.

## Verdicts

Every row was verified by reading the cited code. "platform" in the confidence
column means the defect also depends on documented third-party behaviour.

### High

| id | verdict | where | one line |
|---|---|---|---|
| H1 | confirmed | `oag-proto/src/openai.rs:332` | array-form system message dropped |
| H2 | confirmed | `openai.rs:576`, `:471` | `refusal` never read; refusal reads as empty and escalates |
| H3 | confirmed | `anthropic.rs:43`, `gemini.rs:72` | `thinking_effort` not bridged to a budget |
| H4 | confirmed, regression | `gateway/mod.rs:1167`, `:354`, `:462` | three ledger writes awaited inline |
| H5 | confirmed, regression | `gateway/mod.rs:1167` | lost row displaces the served row under the `request_id` key |
| H6 | confirmed, regression | `lib.rs:383` vs `:400` | spend reconcile spawns only in single-listener mode |
| H7 | confirmed | `eventstream.rs:189`, `bedrock.rs:184` | exception frame has no `type`, falls to `_ => vec![]` |
| H8 | confirmed | `config.rs:117`, `:147` | derived `Debug` prints database and Redis URLs |
| H9 | confirmed | `oag/src/admin/mod.rs:1138` | `INSERT … SELECT` row count discarded; key printed regardless |
| H10 | confirmed | `deploy/envoy/envoy.yaml:97` | `/health/ready` is admin-only; 8080 serves `/health/live` |
| H11 | confirmed | `stacks/aws-fargate/main.tf:188` | bare secret ARN, no version, no new task revision |
| H12 | confirmed | `templates/deployment.yaml:27` | `checksum/config` only, no `checksum/secret` |
| H13 | confirmed, regression | `deploy/test/local-verify.sh:93` | no `since` mark; stale row satisfies the loop |

### Medium

| id | verdict | where | one line |
|---|---|---|---|
| P4 | confirmed | `canonical.rs:346` | `signal()` ignores `thinking_effort` |
| P5 | confirmed | `anthropic.rs:624` | `message_delta` carries `output_tokens` only |
| P6 | confirmed | `openai.rs:818`, `gemini.rs:592`, `responses.rs:1070` | collected converters drop thinking blocks |
| P7 | confirmed | `openai.rs:527`, `:615` | tool-call stop reason not normalised for Chat Completions |
| P8 | confirmed | `stream.rs:255`, `openai.rs:605` | empty argument buffer judged malformed |
| P9 | confirmed | `gemini.rs:627` | prompt count omits cache writes; total includes them |
| P10 | confirmed | `gemini.rs:131` | tool result named by opaque id, not function name |
| P11 | confirmed | `openai.rs:58` | only `max_tokens` emitted |
| G2 | confirmed | `sse.rs:356` | in-band error not extracted in `pump`; second frame emitted |
| S1 | confirmed | `repo.rs:1080`, `:689` | window bounds inside `FILTER`, none in `WHERE` |
| S2 | confirmed | `repo.rs:1152` | multi-row UPDATE read with `fetch_optional`; no unique index on prefix |
| S3 | **refuted 2026-09-06** | `db.rs:59` | premise false: sqlx sends `TimeZone=UTC` in its startup packet, so `pg_settings.source` for it reads `client` and the server's default never applies. Proved against a database forced to `Pacific/Auckland`. No code change; the assumption is now asserted by `the_session_timezone_is_utc_whatever_the_server_prefers` |
| R1 | confirmed | `error.rs:300` | four dispositions never consulted by the selection error path |
| R2 | confirmed | `catalog.rs:208`, `policy.rs:354` | `dearest_served` wins over the ladder ceiling on a partial served set |
| R3 | confirmed | `config.rs:491` | no `client_write_timeout < max_stream_duration` check |
| R4 | confirmed | `config.rs:283`, `mod.rs:1244` | `failover_budget: 0` means one credential |
| R5 | confirmed | `sticky.rs:117` | pin keyed by route, overwritten across providers |
| U2 | confirmed | `codex.rs:203` | `served_models` client has no timeout; poller is serial |
| U3 | confirmed | `pricing/mod.rs:37` | no `CredentialKind` in the price fetch |
| U4 | confirmed (platform) | `anthropic.rs:54` and three siblings | credential not trimmed on the request side |
| U5 | confirmed | `anthropic.rs:37`, `openai.rs:72`, `gemini.rs:46`, `bedrock.rs:150` | base URL joined with no normalisation |
| U6 | confirmed | `bedrock.rs:66` | `host()` keeps the path |
| A2 | confirmed | `admin/auth.rs:57` | every auth error becomes 401, `Overloaded` included |
| A3 | confirmed | `admin/mod.rs:350`, `:428` | two sections degrade to `[]` with no marker in the payload |
| A4 | confirmed | `write.rs:153` | `COALESCE` keeps the cap; comment and sibling say otherwise |
| C3 | confirmed | `oag/src/admin/mod.rs:1497` | `account_route` insert affects zero rows silently |
| C4 | confirmed | `admin/mod.rs:1731` | seat rows counted in the CLI headline |
| C5 | confirmed | `admin/mod.rs:1033` | `init` changes a budget with no eviction |
| C6 | confirmed | `admin/mod.rs:1061` | `role = EXCLUDED.role` promotes on conflict |
| C7 | confirmed | `admin/mod.rs:705` | admin key minted for a non-admin principal, no warning |
| C8 | confirmed (platform) | `admin/mod.rs:186` | env-supplied secret conflicts with `--from` |
| C9 | confirmed | `admin/mod.rs:1666`, `cache.rs:474` | eviction outcome swallowed, success printed |
| C10 | confirmed | `doctor.rs:163`, `:244` | owner binding never selected |
| C11 | confirmed | `usage_import.rs:390`, `:1091` | session id falls back to the filename inside the idempotency key |
| D4 | confirmed | `data-azure/main.tf:48` | `public_network_access_enabled` left at default |
| D5 | confirmed (platform) | `compute-containerapps/main.tf:30` | no `workload_profile_name` on the app |
| D6 | confirmed | `stacks/gcp-cloudrun/main.tf:72` | default deletion policy destroys the pinned version |
| D7 | confirmed | `compute-fargate/main.tf:164`, `:62` | container and ALB probes swapped |
| D8 | confirmed | `data-incluster.yaml:8`, `:23`, `:82`, `:97` | pre-upgrade hooks with default delete policy |
| D9 | confirmed | `_helpers.tpl:56` | PDB guard nested under `not autoscaling.enabled` |
| D10 | confirmed | `templates/configmap.yaml` | fifteen keys, no `extraEnv`, `max_body_bytes` unreachable |
| D11 | confirmed | `edge-cloudflare/main.tf:36` | keepalive variable reaches the guard, not the gateway |
| D12 | confirmed | `.gitignore:12` | `.terraform.lock.hcl` ignored; constraints unbounded |
| D13 | confirmed | `values.yaml:8`, `Chart.yaml:6` | default tag `0.1.0`, zero git tags |
| D14 | confirmed (platform) | `stacks/gcp-cloudrun/main.tf:172` | no domain mapping for the Cloudflare hostname |
| D15 | confirmed | `.github/workflows/` | no Terraform validation, no `cargo audit` |
| X2 | confirmed | `kind-verify.sh:262` | unbounded `count(*)` on a reused cluster |
| X3 | confirmed | `bedrock-verify.sh:145` | `grep bedrock` matches the model name |
| X4 | confirmed | `dialects-verify.sh:167` | passes on `[DONE]` alone |
| X5 | confirmed | `ERRORS.md:135` | names `MOCK_FAIL_FIRST`; harness uses `MOCK_FAIL_STATUS=408` |
| X6 | confirmed | `ERRORS.md:91`, `errors.hurl:42` | `no_viable_model` asserted as `exists`; `not_found` never exercised |
| X7 | confirmed | `docs/08-clients.md:292` | documents rows the `request_id` key drops |
| X9 | confirmed | `deploy/test/` | Responses ingress, half the dialect matrix, failover, escalation, budgets untested end to end |

### Low

| id | verdict | where | one line |
|---|---|---|---|
| P12 | confirmed | `gemini.rs:303` | tool result re-wrapped on every round trip |
| P13 | confirmed | `anthropic.rs:369` | `ToolUseEnd` emitted for text blocks |
| G3 | confirmed | `mod.rs:366` | streamed escalation records no triggering gate |
| G4 (+R11) | confirmed | `mod.rs:444` | suppressed counter fires for non-budget cases |
| G5 | confirmed | `mod.rs:748` | invalid header discards the body pin |
| G6 (+U10) | confirmed | `refresh.rs:48` | refresher chosen by provider, not `adapter_for` |
| G7 | confirmed | `mod.rs:1302` | probe claimed once, retries unchecked |
| G8 | confirmed | `select.rs:24`, `:1014` | `SLOT_TTL` unbound; test compares constants |
| S4 | confirmed | `0001_baseline.sql:156` | `numeric(14,6)` counters vs `numeric(14,8)` ledger |
| S5 | confirmed | `rows.rs:141` | `key_hash` written to L2, never read |
| S6 | confirmed | `0013:12` | index serves no query |
| S7 | confirmed | `repo.rs:97` | `expires_at` checked only at load |
| S8 | confirmed | `cache.rs:242` | `from_secs_f64` can panic on a stored value |
| S9 | confirmed (construction) | `repo.rs:2384` | race test has no forced interleaving |
| R6 | confirmed | `schedule.rs:61` | `waiting` always zero |
| R7 | confirmed | `catalog.rs:294` | allocation per resolve |
| R8 | confirmed (construction) | `catalog.rs:372` | tie-break test iterates one set |
| R9 | confirmed | `sticky.rs:82`, `:96` | fallback form not principal-scoped; doc says every form is |
| R10 | confirmed | `policy.rs:73` | `hard_stop_multiple` unclamped |
| R12 | confirmed | `config.rs:291`, `usage_poll.rs:21` | zero disables every reserve; doc silent |
| R13 | confirmed | `config.rs:523` | "four fields" is twelve |
| U7 | confirmed | six sites | `Client::new()` per request, can panic |
| U8 | confirmed | `sigv4.rs:27` | derived `Debug` over the AWS secret |
| U9 | confirmed | `xai_oauth.rs:178` | `token_endpoint` unvalidated |
| U11 | confirmed | `sigv4.rs:185` | `unreachable!()` on the request path |
| U12 | confirmed | five client builders | `proxy_url` ignored off the inference path |
| U13 | confirmed | `anthropic.rs:48` | OAuth branch unreachable, no `refresh` |
| U14 | confirmed | `eventstream.rs:202` | Latin-1 decode |
| U15 | confirmed | `bedrock.rs:439` | `SignedHeaders=host;` is a constant |
| A5 | confirmed | `metrics.rs:99` | gauge described, never set |
| A6 | confirmed | `metrics.rs:32` | three counters never described |
| A7 | confirmed | `admin/mod.rs:406` | grouped by name; mixed counterfactuals |
| A8 | confirmed | `lib.rs:262` | `/health/ready` outside the ceiling, costs a connection |
| A9 | confirmed | `lib.rs:880` | passes on 404 |
| A10 | confirmed | `lib.rs:268`, `:291` | "three" is four |
| A11 | confirmed | `services.rs:242` | fallback never sets `last_ok` |
| C12 | confirmed | `doctor.rs:291` | returns 0 on every path; tests assert 0 |
| C13 | confirmed | `admin/mod.rs:961` | empty-after-filter reads as empty catalog |
| C14 | confirmed | `admin/mod.rs:1452` | no unique name; no rename or remove command |
| C15 | confirmed | `usage_import.rs:829` | last-write-wins on ambiguous names |
| C16 | confirmed | `doctor.rs:48` | counts rows, ignores versions and `success` |
| D16 | confirmed | `data-neutral/main.tf:25` | TLS check gated on `upstash.io` |
| D17 | confirmed | `compute-fargate/main.tf:217` | invented 120s cap |
| D18 | confirmed | `aws-fargate/variables.tf:24`, `module:34` | `public_subnet_ids` always used |
| D19 | confirmed | `compute-cloudrun/variables.tf:60` | 512Mi against a 1Gi-sized ceiling |
| D20 | confirmed | `prometheus/alerts.yml` | `replica` label; no `PrometheusRule` |
| D21 | confirmed | `floci/deploy.sh:113` | no success flag; `curl -s` |
| D22 | confirmed | `data-aws/main.tf:75` | no `auth_token` |
| D23 | confirmed | various | dead matcher, unused rate-limit var, missing Azure `LOG_JSON`, service `deletion_protection` |
| X8 | confirmed, downgraded | `helm-render-verify.sh:27` | comment claims a chart check that does not exist |
| X11 | confirmed | `ERRORS.md:140` | 18 vs 20 |
| X12 | confirmed | `01-deployment.md:242` | six vs thirteen |
| X13 | confirmed | `README.md:100` | 495 vs 733 |
| X14 | confirmed | `Caddyfile:107`, `stack.yml:94` | port already published |
| X15 | confirmed | `ERRORS.md:110`, `mock-upstream.py:138` | recipe does not reach the watchdog |
| X16 | confirmed | `local-verify.sh:49`, `:103` | no post-loop assert; guard `< 6` vs 7-unpack |
| X17 | confirmed | `07-running-locally.md:30` | three vs four |

## Where my check was weaker than the lane's

A5 and A6: my own extraction of the described metric names failed, so for
these two I relied on the lane's grep of `metrics.rs` plus my own confirmation
that no `gauge!("oag_credentials_schedulable")` call exists anywhere in the
tree. Every other row above was verified by reading the cited code directly.
