---
name: impeccable-rust
description: >-
  Use when writing, reviewing, hardening, or designing Rust for high-stakes or
  long-lived crates, or when auditing how a crate is verified (Miri, Loom,
  Kani, TLA+, Lean). Checklist for exhaustive testing, trustworthy benchmarks,
  data layout, misuse-resistant APIs and everyday API idioms, decision records,
  semver hygiene, deliberate dependency maintenance, and risk-driven formal
  verification with anti-drift rules.
---

# Impeccable Rust

Faults may still happen outside the code (spec, hardware, ops). The
implementation should not be what is to blame. There is no single trick. Heavy
tools cost time and compute; spend a deliberate risk budget where failure
hurts.

## Operating rules

1. Prefer making incorrect use inexpressible over documenting "do not do that."
2. Treat "it works" as insufficient. Show it is not broken under chaos and edge cases.
3. Prefer automation that catches human misses (Miri, Loom, Kani, semver checks, cargo-vet).
4. When you accept a downside or skip a corner case, write it down.
5. Stagnation is a choice with rising cost. Surface it; do not silently defer forever.
6. Apply expensive verification where failure actually hurts.
7. Give every important failure mode an owner. Link any second model to production Rust before treating its results as evidence about the Rust.

## Checklist (run what applies)

Work through each section that fits the change. Skip sections that clearly do
not apply (for example, no concurrency means skip Loom). Say what you ran and
what you deliberately skipped. A new verifier, a second semantic model, or a
concurrency, recovery, or persistence change also follows Verification
architecture.

### 1. Testing

- Assert invariants. Panics on broken assumptions beat silent wrongness.
- It works is not the same as it is not broken.
- Run Miri on tests that touch `unsafe`, custom allocators, or subtle provenance (`cargo +nightly miri test`).
- Use sanitizers (nightly `-Zsanitizer=address` or `thread`) when threading, memory access, or FFI matters beyond what Miri can run.
- Test error paths, not only the happy path.
- Error litmus test: temporarily replace `return Err(...)` with `continue` inside a loop (or otherwise skip the error return). If the suite still passes, error-path coverage is broken. Tests must trigger and verify the exact `Err`.

### 2. Chaos

Add at least one chaos layer that fits:

| Kind | Tools |
|------|--------|
| Thread and task schedules | `shuttle` |
| Network faults, partitions, crashes | `turmoil` |
| Value | `quickcheck`, `proptest` |
| Logic | `cargo-mutants` |

For reimplementations (custom map, codec, parser), property-test against a trusted oracle (for example, std) and assert broad invariants such as "never panics."

### 3. Exhaustive verification

- Loom for the distinguishable concurrent executions of lock-free, atomic, or custom sync code. Its memory model is partial (no load buffering, `SeqCst` treated as `AcqRel`), so state that limit with the claim.
- Kani for symbolic / model-checked inputs around `unsafe`, and on high-risk logic whose named property must hold for every input.

Keep exhaustive tools on the smallest core that must be correct. Loom owns that small concurrent implementation. Kani owns symbolic inputs on that core. System interleavings, deadlock, liveness, and recovery architecture are assigned under Verification architecture.

### 4. Benchmarks

Cover the full performance profile:

- Pathological cases
- Micro and end-to-end
- Under, at, and over capacity
- All relevant targets

Trustworthy measurements (CI should fail on regression):

- Prefer instruction-count / callgrind-style metrics over wall time alone (for example, `gungraun`, formerly `iai-callgrind`).
- Interleave old and new (for example, `tango-bench`) to cut noise.
- Use a dedicated host and leave headroom (under 100% load).

Measure what matters, not only speed:

- Throughput and goodput (a flood of 500 responses can show high throughput with zero useful work)
- Memory (average and max)
- Latency distributions, not only the mean
- Outcomes: realistic inputs, measured outputs, compare to ground truth
- Prefer the real deployment target, not only a beefy CI box

Record how you load the system (open, closed, partly-open), which statistic you report (mean, median, histogram, CDF), and how you decide a regression. "y is greater than x" is not enough.

#### Data layout

Measure first. Apply these only when a profile shows the hot path is memory- or cache-bound.

- Prefer a `u32` index into a contiguous arena over a `Box` or `Rc` pointer graph, and a generational handle (for example, `slotmap`) once slots are reused. Neighbors stay on the same cache lines, and a generation stops a stale id from aliasing a reused slot. Keep real `&T` borrows for a small graph or an API that must hand out references.
- Use struct-of-arrays for a hot loop that touches a field subset, so positions are not dragged along with cold names and flags. Keep array-of-structs when each access needs the whole record, or when the collection is tiny.
- Store a sparse `Option` or `bool` out of band (side map, bitset, or parallel array keyed by id) so a rare field does not widen every hot row. Leave the field in the struct when most rows have it.
- Box a large rare enum variant so the enum is not sized to that arm, and assert `size_of` in tests so a fatter variant fails CI. Skip the box when the variants are similar in size, or when the extra indirection loses in a profile.
- Set `repr` and alignment when layout is part of the contract: `repr(C)` for FFI or stable bytes, `repr(align(128))` or `crossbeam_utils::CachePadded` so a hot atomic does not share a cache line (64 can be too small on x86_64 and aarch64). Leave everyday structs on rustc's default field order. Reach for `packed` only after a measurement shows the padding costs more than unaligned access.

### 5. Documentation

Decisions:

- Alternatives discarded and why
- Downsides accepted and why
- Short ADRs, YADRs, or design notes

What is not there:

- Missing corner-case handling. Tell callers what the code cannot do. A silent `todo!()` is a landmine.
- Known future optimizations
- Deliberate absence of impls (for example, no `From` for a reason)

### 6. Misuse resistance

Make misuse inexpressible:

- Newtypes, not type aliases (`Meters(u64)` vs `Miles(u64)`)
- Two-phase structs (raw `TomlConfig` vs validated `ResolvedConfig`)
- Enums for linked arguments (encode linked `bool` + `Option` pairs as one type so conflicting states cannot be constructed)

State-machine ladder (stop at the first rung that rules out the illegal states):

- Keep `bool` for flags that are independent and not a lifecycle (`verbose` and `color`).
- Turn lifecycle bools and phase-only `Option`s into an enum, including multi-bool parameter lists. Put each field on the variant that owns it so ghost data and combinations such as connected-but-not-open cannot be built.
- Nest when a flat enum copies the same fields onto many variants and every match names every micro-state (past a handful of simple variants). Share context on an orchestrator and delegate to a phase enum (`Session::Auth { ctx, phase }` and `Session::Work { ctx, phase }`).
- Use typestate when the next call must be impossible until the transition runs (`Rocket<Ground>` vs `Rocket<Air>`). Consume `self` so the old state cannot be reused. Stay on a runtime enum if you need one `Vec` of mixed states, or if the phase is data from outside the API.

Idioms:

- Clippy in CI (deny or warn meaningfully)
- Rust API Guidelines
- If an API smells like OOP factories and inheritance trees, redesign toward builders, ownership-honest APIs, and few trait-supertrait pyramids
- In an immutable API, return `Option<&T>` rather than `&Option<T>`. Use `as_deref` when the stored value is `Box<T>` or `String`. Callers can map, filter, or yield a computed `None` without depending on storage, and `Option<&T>` is pointer-sized.
- Take `&mut Option<T>` only when the callee inserts or clears the variant. Take `Option<&mut T>` to edit a present value, and `Option<T>` by value when the callee needs ownership.
- Do not make callers go through your `Deref` container. Return `&str`, `&[T]`, `T`, or `impl AsRef<_>` for the value they need.
- Take `impl Into<T>` at a public edge when the conversion is the ergonomic point. On a hot or internal path, take the concrete type so the allocation and the inference stay obvious.
- A semicolon turns a tail expression into `()`. When a `match` or `if` arm should produce a value, leave it as an expression (`0 => { "zero" }`, not `0 => { "zero"; }`).
- Deny `unsafe_op_in_unsafe_fn`, so unsafe operations inside an `unsafe fn` still need an explicit `unsafe` block.
- For long-lived immutable shared data, store `Arc<[T]>` or `Arc<str>` (`Rc<[T]>` on one thread, `Box<[T]>` for a single owner). Build in `Vec` or `String`, then freeze. `Arc<String>` and `Arc<Vec<_>>` keep a spare capacity field and an extra pointer hop.
- Use `Option` when absence is the whole story, and `Result` when the caller must branch on why. Short-circuit a fallible iterator with `collect::<Result<Vec<_>, _>>()`.
- When a plugin or strategy trait must be `dyn` and the method is async, return `Pin<Box<dyn Future<Output = ...> + Send + 'a>>`. Prefer a static `async fn` in the trait when dynamic dispatch is not required. In a public trait, write `fn run(&self) -> impl Future<Output = T> + Send` so callers can rely on `Send`.

### 7. Compatibility

Public surface area is a liability. Prefer:

- `-> impl Trait` over naming concrete return types you may want to change (auto traits such as `Send` still leak through it)
- Private fields with accessors or builders instead of `pub` fields
- No leaking public dependencies in args, returns, trait impls, or re-exports
- Non-pub inherent methods over blanket `impl From` / always-public trait impls when the coupling is accidental

Automate:

- `cargo-semver-checks`
- `cargo-public-api`

Keep a simple, stable core. Document semver expectations for callers.

### 8. Dependencies

1. Track the complete dependency closure across every deployment that matters.
2. Join against known issues (for example, RUSTSEC).
3. Vet for unknown issues (`cargo-vet`, public or internal).

Be able to answer operational questions such as which deployed units still run a vulnerable transitive crate.

### 9. Stagnation as a choice

Loud reminders when you are behind or dependencies are dead (Dependabot / Renovate). Reduce friction:

- Auto-merge dependency bump PRs that pass tests
- Budgeted maintenance time
- Prefer upstreaming over long-lived forks
- Wrap unstable dependencies behind a stable internal facade
- Treat rustc / edition lag the same as crate lag

Cost rises with every skipped upgrade cycle.

## Verification architecture

> Every important failure mode has an owner, every verifier has a reason to exist, claims match their limits, production Rust is checked against the intended semantics, and independent models cannot silently drift.

More tools are not a stronger architecture. Inspect the crate, assign an owner to each real failure, and add a model only when the Rust checks cannot own that failure.

### Probe before recommending

- Read the workspace before naming a tool: `Cargo.toml` and workspace members, `src/`, `crates/`, `tests/`, `benches/`, `fuzz/`, `scripts/`, `.github/workflows/` or other CI and task files (`xtask`, `justfile`, `Makefile`), `AGENTS.md`, `CONTRIBUTING.md`, `docs/`, and any `formal/`, `spec/`, or `proof/` tree.
- Search for state machines, reducers, events, commands, effects, workers, schedulers, queues, retry, cancel, timeouts, recovery, journals, replay, transactions, persistence, locks, atomics, channels, spawn, `unsafe`, FFI, protocols, parsers, and serialization.
- Record verifiers already present: proptest, quickcheck, cargo-fuzz, cargo-mutants, Loom, shuttle, turmoil, Miri, sanitizers, Kani, Verus, Creusot, TLA+, TLC, Lean, Rocq (Coq), Alloy, and any written specification or model check.
- Count a tool as an owner only where it runs, in CI or another enforced gate, against that failure class. Recommend another only when a failure class below has no owner.

### Risk to owner

Assign every failure class the crate actually has. This is a decision table, not a stack to install.

| Failure class | Preferred owner |
|---------------|-----------------|
| Deterministic logic | Unit, property, or differential tests |
| Untrusted input | Fuzz |
| Unsafe or provenance | Miri, plus fuzz or Kani; sanitizers for FFI and other code Miri cannot run |
| Small concurrent implementation | Loom |
| System interleavings, deadlock, liveness, or recovery architecture | TLA+ for the design; `turmoil` or `shuttle` for the Rust that implements it |
| Crash persistence | Crash and fault tests; add TLA+ when recovery architecture is the risk |
| Mathematical kernel | Lean, Verus, or Kani when a named property justifies it |
| Several DSLs or frontends | Differential or conformance tests |
| Public API break | `cargo-semver-checks`, `cargo-public-api` |
| Vulnerable or unvetted dependency (any crate with dependencies) | `cargo deny` / RUSTSEC, `cargo-vet` |
| Performance regression | Benchmark gate on non-noisy metrics |

Deterministic logic stays on tests. It becomes a mathematical kernel only when a named property must hold for every input and tests cannot close it. Add a formal tool only for a row whose preferred owner is that tool.

### TLA+

- Recommend TLA+ when correctness depends on how multiple actors interleave: workers, schedulers, queues, ownership handoff, retry, timeout, cancellation, crashes, recovery, distributed state, deadlock freedom, or liveness.
- TLA+ owns that system-level design. A retry or timeout inside one task, or a trivial deterministic function, stays on Rust tests and simulation.
- When a model exists, document its state variables, actions, invariants, liveness properties, fairness assumptions, bounds, abstractions, and the production Rust each piece maps to.
- TLC exhaustively enumerates the finite instance its configuration sets; its simulation mode only samples, and Apalache checks to a depth bound. State the bounds. None of these runs is an unrestricted proof; a checked TLAPS proof is.

### Lean and other provers

- Add Lean, or a similar prover, only for a small kernel where a theorem is the point: replay algebra, effect identity or uniqueness, normalization, monotonicity, ordering, ranking, a scheduler algorithm, capability or policy composition, or another deterministic transformation that tests do not close.
- Every artifact answers one question: what theorem does this establish that Rust tests do not? A weak answer means the prover is not justified.
- Skip a Lean enum or step function whose only job is to mirror a Rust enum or reducer.

### Semantic duplication

- Flag a Rust reducer, a TLA+ transition relation, a Lean step function, and a fixture interpreter that encode the same steps. Four green suites can still be four different semantics.
- Classify each extra model as a necessary abstraction, a formal specification, a useful differential implementation, accidental duplication, or verification theater.
- A necessary abstraction drops detail so a different failure class can be checked, and a conformance link says what was dropped. Theater is a green run with no owner, no stated bounds, and no link to production Rust.

### Conformance

- When more than one model is necessary, pin them with a canonical transition corpus, model-generated traces, trace replay, property-based equivalence, differential execution, shared fixtures, canonical serialization, or runtime assertions.
- Prefer records of `initial_state`, `event`, `expected_next_state`, and `expected_effects`, or the sequence form `initial_state`, `events[]`, `expected_states[]`, `expected_effects[]`.
- Production Rust should execute those traces when that is practical. Require that executed link before treating similar sources, or two green suites, as the same semantics.

### DSL and workflow

- When the crate has a workflow language or DSL, verify its meaning at the compile step into a shared Rust IR. The frontend is an optional producer of that IR.
- Rust owns I/O, effects, networking, persistence, scheduling, workers, resource management, and runtime recovery.
- A verified frontend does not become the runtime architecture.

### Anti-drift

Follow this block on every change. In implementation mode, also copy the whole block into `AGENTS.md` or `CONTRIBUTING.md`:

> Any change to observable semantics names the verification boundary it affects.
>
> - Concurrency, interleaving, scheduling, retry, cancellation, recovery, ownership handoff, or liveness updates the system model, or the change states why that model is unaffected.
> - Executable Rust behavior updates the Rust verification layer. A theorem-owned kernel updates its proof. Workflow or DSL semantics update conformance or differential tests.
> - Do not clone one state machine across Rust, TLA+, Lean, and a DSL for symmetry. Passing independent suites does not establish equivalence.

### Verification impact

Put this declaration in the PR description of a change to observable semantics. Adapt it to the repository and omit boxes the crate cannot affect.

```text
Verification impact

[ ] Pure Rust deterministic behavior
[ ] Concurrency / interleaving behavior
[ ] Distributed / system state model
[ ] Crash / recovery / replay behavior
[ ] Persistence semantics
[ ] TLA+ model
[ ] Mathematical proof kernel
[ ] Workflow / DSL semantics
[ ] Unsafe / memory behavior
[ ] Property-test / fuzz surface
[ ] No verification architecture impact

Reason:
Affected invariants:
Tests or proofs updated:
```

### Modes

Audit mode is the default for a verification review, and for any verifier or formal model you would add, remove, or replace without being asked. It is read-only: leave the tree unchanged on that first pass. Report:

- Architecture, risk, and current-verifier maps
- Duplication, drift, gaps, and the owner of each important failure
- Each recommendation marked REQUIRED, USEFUL, OPTIONAL, NOT JUSTIFIED, or REMOVE
- A conformance and anti-drift plan, a CI split, and a migration order

A requested code change is not an audit. Make it with the Rust checks that Risk to owner assigns, recommend any new formal model instead of writing it, and fill in Verification impact.

Implementation mode starts when the user accepts that report or asks for the verification change, in this order:

1. Add missing conformance for each model the audit keeps.
2. Add the high-value Rust checks the risk table already names.
3. Write down semantic ownership.
4. Strengthen a formal model only where it owns a real failure.
5. Remove accidental duplication.
6. Simplify CI.

Never remove a verifier until the guarantee that replaces it is named, in place, and passing.

### Terminology

- Name the claim with the narrowest of these that fits: compiler-enforced, type-enforced, unit-tested, integration-tested, property-tested, fuzz-tested, mutation-tested, Miri-checked, sanitizer-checked, model-checked, bounded model-checked, exhaustively enumerated under stated bounds, theorem-proven, differentially tested, conformance-tested, crash-tested, fault-tested, or simulation-tested.
- State the bounds and the assumptions next to the claim.
- If a formal model has no refinement or conformance relationship to production Rust, say so in the report.
- Reserve "proof" for a theorem with stated assumptions. A test suite, a fuzz run, and a bounded model check (including what Kani calls a bounded proof) are not proofs.

## CI shape

Adapt to the crate. Minimum credible set:

1. `cargo test` + Clippy + rustfmt, and deny `unsafe_op_in_unsafe_fn` on crates that contain `unsafe`
2. Miri for `unsafe` / allocator / concurrency-sensitive tests
3. At least one of: proptest/quickcheck, mutants, or fuzz on parsers / codecs
4. Loom and/or Kani gated to the modules that need them
5. Benchmark regression gate with non-noisy metrics
6. `cargo deny` / RUSTSEC audit + optional `cargo-vet`
7. `cargo-semver-checks` on published API crates

Item 1 runs on every crate. Skip any other tool the risk table does not justify, and split the rest by cost:

- Pull request: `cargo fmt --check`, `cargo check`, Clippy, unit and integration tests, property tests that cover the diff, Miri on tests that touch `unsafe`, `cargo deny`, `cargo-semver-checks` on published crates, and the benchmark gate, plus small Loom or Kani runs, small model checks, and conformance tests when those owners exist.
- Nightly: larger fuzz campaigns, broader Miri, large TLC state spaces, fault injection, stress tests, large Loom scenarios, and deterministic simulation.
- Release: the full matrix when a failure class in the risk table justifies the cost.

## Review report

When finishing work under this skill, report:

- Evidence: each check run, or type-level misuse made impossible, named with a Terminology term and its bounds
- Documented: decisions and intentional gaps written down
- Deferred: what was skipped and why (follow-up if high stakes)
- Compat / deps: any new public surface or dependency hazard
- Verification: the Verification impact declaration, the owner of each failure mode this change can break, and any conformance or model update

## Anti-patterns

- Happy-path-only tests
- Wall-clock microbenchmarks on a shared machine as the sole perf signal
- `pub` everything, boolean soup, type aliases for distinct units
- `Arc<String>` or `Arc<Vec<_>>` for data that is already immutable, and read APIs that return `&Option<T>` or a `Deref` newtype
- Leaking hyper / serde / tokio types into a stable public API without intent
- Silent TODO debt and forever-pinned dependency versions with no reminder
- Claiming this quality bar without the checks that apply (Miri on `unsafe`, property tests), misuse-resistant types, or decision docs
- A second copy of the same state machine in TLA+, Lean, or test fixtures with no conformance link to production Rust
- Calling a bounded model check, a fuzz campaign, or a green unit suite a proof
- Adding a prover or a system model to mirror logic that unit and property tests already own
