# Agent and contributor notes

The gate is `just check` (fmt, clippy, tests); CI also runs `cargo audit`,
`cargo deny` and, on pull requests, `cargo mutants` over the diff. Tests that
need Redis or Postgres skip themselves unless `OAG_TEST_REDIS_URL` and
`OAG_TEST_DATABASE_URL` are set -- `just dev-up` and export them before
trusting a green run.

## Who owns which failure

| Failure class | Owner |
|---|---|
| Deterministic logic (router, pool, proto) | unit tests; `cargo mutants` on the diff |
| Request path across dialects | `deploy/test/*-verify.sh` against mocks |
| Store and slot accounting | gated integration tests with real Redis/Postgres |
| Slot admission under concurrent callers | `scripts/claim-slot-race.sh` (manual) -- no automated owner yet |
| Unsafe / memory | none needed: `unsafe_code = "forbid"` workspace-wide |
| Vulnerable, unlicensed or off-registry dependency | `cargo audit`, `cargo deny` (`deny.toml`) |
| Stale dependencies and actions | Dependabot (`.github/dependabot.yml`) |
| Infrastructure | `terraform validate`, `deploy/test/tofu-verify.sh`, the k8s workflow |

No crate is published, so there is no semver gate. Actions are pinned to
commit SHAs; bump them through Dependabot, not by hand-editing a tag.

## Anti-drift

> Any change to observable semantics names the verification boundary it affects.
>
> - Concurrency, interleaving, scheduling, retry, cancellation, recovery, ownership handoff, or liveness updates the system model, or the change states why that model is unaffected.
> - Executable Rust behavior updates the Rust verification layer. A theorem-owned kernel updates its proof. Workflow or DSL semantics update conformance or differential tests.
> - Do not clone one state machine across Rust, TLA+, Lean, and a DSL for symmetry. Passing independent suites does not establish equivalence.

This repository has no TLA+ or Lean model; "the system model" here is the
integration and race tests above. Fill in the Verification impact section of
the PR template on any change to observable behaviour.
