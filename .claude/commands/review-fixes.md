Execute the fixes from the review of the remediation. `$ARGUMENTS` says what to
do; empty means `next`.

- `status` — report where things stand and stop.
- `next` — find the first incomplete item and do it, end to end.
- One or more item ids (`B1 B2`, `C6`, `P3`) — do exactly those.
- `stage 1` / `stage 2` — do every remaining item in that stage.

You are executing a settled plan. Do not re-review the code for new findings,
do not reorder the stages, and do not widen a fix beyond its item.

## The sources of truth

| what | where |
|---|---|
| **the plan — read it first, in full** | `docs/research/remediation-review-2026-09-06-plan.md` |
| the findings behind it, with severity and evidence | `docs/research/remediation-review-2026-09-06.md` |
| the four decisions the owner already made | top of the findings document — they are settled |
| the original review, for a finding's full text | `docs/research/review-full-2026-09-05.md` |
| what is done | `git` and the PR bodies — never this file's memory of it |

Every item in the plan has six parts: WRONG, WHERE, PROVE IT, FIX, TEST,
VERIFY. **Run PROVE IT before touching anything.** If it does not reproduce,
the item is a refutation: record the evidence in the PR body and move on.

## Where things stand

Determine this from git at the start of every run: which `review/group-*`
branches exist, what each PR body says under "Post-review fixes", whether
`review/group-7-review-fixes` exists.

- **Stage 1** — eight blockers (B1–B8), each onto the branch that introduced
  it, then merged upward. Nothing has started.
- **Stage 2** — one follow-up PR on `review/group-6-tests-docs`: D8, C1–C9,
  P2, P3, P4. Nothing has started.

## The loop, per item

1. Read the item in the plan. Read the finding it cites.
2. Check out the branch the item names. `git pull`.
3. **PROVE IT.** Paste the output into your notes; it goes in the commit body.
4. Make the change. Fix any comment the change makes false, in the same commit.
5. Write the test. Run it: it passes.
6. **Revert-check, both ways.** Total: stash the fix, test fails, pop.
   Partial: keep the helper, delete only the call site, test fails, restore.
   A test that survives either is a defect in the test — fix the test.
7. **VERIFY** per the item, then the gate from §0 of the plan. Nothing piped
   through `tail`.
8. Commit, one item per commit, with the plan's conventions.
9. For Stage 1: merge upward through every branch above, push all of them.
   For Stage 2: push `review/group-7-review-fixes`.
10. Report, then stop — unless `$ARGUMENTS` named more than one item or a stage.

## Rules that do not bend

Section 0 of the plan, in full. The ones that have cost the most, restated:

- `OAG_TEST_DATABASE_URL` and `OAG_DATABASE__URL` name **`oag_g0`**, never
  `oag`. The `oag` database is live and its storage is tmpfs.
- Never `just dev-down`, `just dev-reset`, `DROP DATABASE`, `CREATE DATABASE`.
- Never paste a key, token or password anywhere.
- Pid **98283** is the live opengrok gateway. Kill only `oag serve` processes
  whose elapsed time is minutes (`MM:SS`).
- B3/B4: check the Anthropic docs **first**. If `budget_tokens` is rejected on
  Claude 5, stop and report.
- D8: the hook change and the `helm upgrade` test land in the **same commit**.
- Nothing creates a cloud resource. `terraform validate` and `tofu-verify.sh`
  are the deploy gate.
- Merging is the owner's call. Green CI is the precondition, not the trigger.

## The report at the end of an item

Short. The item id and branch; PROVE IT output before; the test name; both
revert-check outcomes; VERIFY output after; the commit sha; anything that
needs the owner, as the exact command or decision. Then one line: what the
next run will do.
