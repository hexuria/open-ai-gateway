# The ledger at scale: rollups first, partitions when retention is decided

Design pass for plan item 6.5 of the audit remediation: "monthly range
partitioning on `occurred_at` plus a rollup table for the dashboard". Written
2026-09-05 against `main` at `5776152`. Nothing here is implemented.

## The short version

`usage_event` is the billing record. Every dashboard aggregate, every usage
window and the monthly-spend reconcile are time-range scans over it, and
nothing prunes it. Three things follow, in order of payoff per unit of risk:

1. **Fix the predicates.** Four dashboard queries use `($1 IS NULL OR
   occurred_at >= $1)`, which Postgres cannot serve from the time index once the
   prepared statement goes generic. Rewriting them as a plain range is a
   one-afternoon change with no schema impact and makes every bounded period
   use the index it already has. Not done in the remediation; do it first.
2. **Add a daily rollup table.** `period=all` and every wide period must read
   the whole ledger today, however it is indexed or partitioned. A rollup keyed
   on day and the dimensions the dashboard groups by turns those into reads of
   a few thousand rows. Maintained incrementally from a watermark by the same
   kind of task as the spend reconcile, never on the request path.
3. **Partition, but only once a retention policy exists.** Partition pruning
   buys almost nothing over the existing `(dimension, occurred_at DESC)` indexes
   for the queries this system runs. What partitioning is for is dropping a
   month in one statement instead of deleting millions of rows, and that is
   only useful if there is a month you are allowed to drop. Deciding that is a
   product and compliance question. If the answer is "keep everything", stop
   after step 2.

The rest of this document is the detail behind each step, and the migration
path for step 3, which is the part that can go wrong.

## What the ledger is asked

Every reader of `usage_event`, grouped by shape. Line numbers are indicative.

| Reader | Shape | Bounded by |
|---|---|---|
| `admin::summary` totals, by-tier | `SUM` over a period | `occurred_at` range, or unbounded for `period=all` |
| `admin::seat_summaries` | per-seat `SUM` over a period | `account_id` + range; drives from the handful of oauth accounts |
| `admin::origin_breakdown` | `GROUP BY origin, account` over a period | range only |
| `admin::usage` listing | newest N rows | `ORDER BY occurred_at DESC LIMIT` |
| `repo::key_usage`, `usage_by_model`, `points_for_keys` | per-key sums over 5h / 24h / 7d / month windows | `api_key_id` + range |
| `repo::principal_usage` | per-principal month sum | `principal_id` + range |
| `repo::reconcile_monthly_spend` | per-budgeted-row month sum | `principal_id` or `route_id` + range |
| `repo::gateway_fingerprints`, `gateway_activity` | importer dedup probes | `origin` + range |
| `oag admin` month report | month sum | range |

Two observations. Every query but the listing is a time range. And every
query with a dimension already has a matching `(dimension, occurred_at DESC)`
index (`0001`, `0008`, `0009`), so for a bounded window the planner reads only
that dimension's slice of the window. The only queries that must read the whole
table are the unbounded dashboard periods, and no index or partition helps
those.

The request path touches the ledger exactly once per attempt, in
`record_usage`, as an insert plus the three counter debits in one statement.
That write is what every design below has to keep cheap and idempotent.

## What the ledger promises

Three constraints carry meaning beyond storage, and partitioning changes all
three:

- **`PRIMARY KEY (request_id)`** on the base table. `record_usage` inserts with
  an untargeted `ON CONFLICT DO NOTHING`, which is what makes a retried write
  for the same request a no-op instead of a second debit. Two tests pin the
  constraint's exact definition, because the previous release inserts against
  it during every rolling deploy.
- **`UNIQUE (request_id, attempt)`** from `0003`, the key the ledger is meant to
  contract onto so an abandoned or lost attempt and the served one can coexist.
  Still pending: the primary key above still drops the second row.
- **`UNIQUE (source_ref) WHERE source_ref IS NOT NULL`** from `0008`, the
  importer's idempotency key; the importer also derives `request_id`
  deterministically from the same string.

A Postgres partitioned table requires the partition key in every primary key
and unique constraint. Partitioning on `occurred_at` therefore turns each of
these into `(…, occurred_at)`, and that quietly breaks the promise: a retried
`record_usage` inserts with `occurred_at DEFAULT now()`, so the retry no longer
conflicts and the key is debited twice. This is the central hazard of step 3
and the reason it needs its own identity table (below), not just a `PARTITION
BY` clause.

## Step 1: predicates the index can use

`summary` totals, `by_tier`, `seat_summaries` and `origin_breakdown` bound the
window as `($1::timestamptz IS NULL OR occurred_at >= $1)`. sqlx prepares the
statement, Postgres switches to a generic plan after a few executions, and a
generic plan cannot fold the `IS NULL` away, so the disjunction is not an index
condition and `usage_event_time_idx` goes unused even for `period=today`.

Rewrite every one of them as a closed range with sentinels:

```sql
WHERE occurred_at >= COALESCE($1, '-infinity'::timestamptz)
  AND occurred_at <  COALESCE($2,  'infinity'::timestamptz)
```

Eight occurrences, all in `crates/oag-server/src/admin/mod.rs` and one in
`repo.rs`. `by_tier` also wants `CREATE INDEX usage_event_tier_time_idx ON
usage_event (tier, occurred_at DESC)`, the one dashboard dimension with no
index. Pin with a test that runs `EXPLAIN` for `period=today` and asserts an
index scan; the store tests already have the Postgres harness for it.

This does nothing for `period=all`. That is step 2's job.

## Step 2: a daily rollup

### Shape

One table, one row per day per combination of the dimensions the dashboard
groups by:

```sql
CREATE TABLE usage_daily (
    day                     date        NOT NULL,
    origin                  text        NOT NULL,
    account_id              uuid,                       -- NULL for unattributed
    tier                    text        NOT NULL,
    seat_shaped             boolean     NOT NULL,       -- cost_usd = 0 AND counterfactual_api_usd > 0
    requests                bigint      NOT NULL DEFAULT 0,
    input_tokens            bigint      NOT NULL DEFAULT 0,
    output_tokens           bigint      NOT NULL DEFAULT 0,
    cache_read_tokens       bigint      NOT NULL DEFAULT 0,
    cache_write_tokens      bigint      NOT NULL DEFAULT 0,
    cost_usd                numeric(16,8) NOT NULL DEFAULT 0,
    counterfactual_usd      numeric(16,8) NOT NULL DEFAULT 0,
    counterfactual_api_usd  numeric(16,8) NOT NULL DEFAULT 0,
    PRIMARY KEY (day, origin, tier, seat_shaped, account_id)
);
```

`seat_shaped` is materialised because `summary` and `seat_summaries` split on
exactly that predicate; keeping it as a column means the headline (`NOT
seat_shaped`) and the seat rows (`seat_shaped`) are both plain filters. Every
sum the four dashboard queries take is a sum over these columns, and `COUNT(*)`
is `SUM(requests)`. A day with a thousand distinct `(origin, tier, account)`
combinations is a thousand rows; a year is under half a million rows for a
large deployment, and a dashboard load reads only the days in its period.

`PRIMARY KEY` with a nullable `account_id` needs the usual treatment: store the
nil uuid for "unattributed" rather than NULL, and translate at the edge.

### Maintenance

Not on the request path. An upsert per request onto the day's row would put
every request in a deployment behind a lock on the same few rows, which is the
hot-row contention the monthly counters avoid by being one row per principal.

A periodic task, shaped like `spawn_spend_reconcile`, with a watermark:

```sql
-- rollup_state: one row, the last occurred_at fully folded in.
INSERT INTO usage_daily (...)
SELECT date_trunc('day', occurred_at)::date, origin, COALESCE(account_id, nil),
       tier, (cost_usd = 0 AND counterfactual_api_usd > 0),
       COUNT(*), SUM(input_tokens), ...
  FROM usage_event
 WHERE occurred_at >  $watermark
   AND occurred_at <= $upto
 GROUP BY 1, 2, 3, 4, 5
ON CONFLICT (day, origin, tier, seat_shaped, account_id) DO UPDATE
   SET requests = usage_daily.requests + EXCLUDED.requests, ...;
UPDATE rollup_state SET folded_through = $upto;
```

Both statements in one transaction, and `$upto` chosen as `now() - 1 minute`
rather than `now()`, because `occurred_at` is assigned at insert and a
transaction that began before the watermark can commit after the pass read.
The one-minute lag is the same trick the seat usage poll relies on. Runs every
five minutes; the first run on an existing ledger folds history in month-sized
batches so a single statement never holds a multi-minute snapshot. The
importer, which writes rows with old `occurred_at`, is the one writer this
watermark misses: it should call the rollup for the range it wrote, or the
importer's rows should be excluded from `usage_daily` (they are already
excluded from the headline by `origin`, so this is a real option).

The reconcile that landed in #62 is the template for the transaction shape and
the task shape, and its tests are the template for the tests: one that folds
an old row in, one that a debit landing during a pass is not lost.

### What reads it

`summary` totals, `by_tier`, `origin_breakdown` and `seat_summaries` for any
period that starts before today, with today's partial day still read from the
ledger and added. The per-key and per-principal windows stay on the ledger:
they are narrow, indexed and hours-scale. The listing stays on the ledger. The
reconcile stays on the ledger, deliberately: it is the thing that keeps the
counters honest, and a rollup is a second derived copy that must not become a
third source of truth.

## Step 3: partitioning, and the retention decision that justifies it

### Why not just partition

For the queries above, monthly partitions on `occurred_at` prune to the same
rows the `(dimension, occurred_at DESC)` indexes already reach. The real gains
are operational: `VACUUM` and index maintenance per partition rather than per
table, and dropping a month as `DROP TABLE` instead of a `DELETE` that bloats
the table and its six indexes. The second is only a gain if a month is ever
dropped. This system has no retention policy, no `DELETE` on the ledger outside
the importer's undo, and a stated view that the ledger is the record. Decide
that first. Reasonable answers:

- **Keep everything.** Stop after step 2. Revisit when the table passes a size
  that hurts, which with the rollup in place is a vacuum and disk question,
  not a query one.
- **Keep N months raw, rollups forever.** Partition, and drop partitions older
  than N once the rollup has folded them. The importer's dedup probes read the
  ledger by `origin` and range, so N must cover the longest re-import anyone
  will run.
- **Keep everything, but cold.** Partition and detach old months to a cheaper
  tablespace or an archive table. Most of the cost of the third option for a
  fraction of the benefit; listed for completeness.

### The identity table

Whichever option, the idempotency promise has to survive. Keep a narrow,
unpartitioned table whose only job is to be the arbiter:

```sql
CREATE TABLE usage_event_id (
    request_id uuid     NOT NULL,
    attempt    smallint NOT NULL DEFAULT 0,
    PRIMARY KEY (request_id, attempt)
);
```

`record_usage` inserts here first inside its CTE, `ON CONFLICT DO NOTHING
RETURNING request_id`, and inserts the ledger row only from what that returned.
The partitioned ledger then needs no cross-partition uniqueness at all; a
partition-local `(request_id, attempt, occurred_at)` key is kept for sanity.
This also completes the `0003` contract onto `(request_id, attempt)` that has
been waiting for the primary key to move: the identity table *is* that key, so
abandoned and lost attempts finally get their own rows. The importer's
`source_ref` uniqueness moves to the same table as a nullable column with a
partial unique index, or stays as a partition-local index plus the
deterministic `request_id` it already derives; the second is enough, since a
re-import lands in the same month as the original.

Rows in the identity table are removed in lockstep with dropped partitions, by
a `DELETE ... WHERE request_id IN (SELECT request_id FROM the_partition)` run
before the drop. Under the "keep everything" option it simply grows with the
ledger, at sixteen bytes a row plus index.

### Migration path

Postgres cannot convert a table in place, and every migration here lands while
the previous release is still inserting into `usage_event` by name for up to
thirty minutes. The path that keeps that release working and never takes a
long exclusive lock is the trigger-and-swap:

**Release A (expand).**
- Create `usage_event_id` and `usage_event_p` (partitioned, monthly, with a
  `DEFAULT` partition for anything unexpected), with the same columns and the
  same six indexes as the ledger plus the identity table.
- Create partitions for the current month and the next twelve. A scheduled
  task creates the month after next on every run so partitions never run out;
  the `DEFAULT` partition catches it if that task dies, and a `doctor` check
  reports rows landing there.
- Install an `AFTER INSERT` trigger on `usage_event` that inserts the new row
  into `usage_event_id` and `usage_event_p`. This is what makes the previous
  release's writes reach the new table without knowing it exists.
- Backfill history into the new tables month by month, oldest first, as an
  idempotent `INSERT ... SELECT ... ON CONFLICT DO NOTHING` per month. Run it
  from `oag migrate` after the sqlx migrations, the way the reconcile runs
  there, with the statement timeout already lifted; on a large ledger it is
  minutes and it blocks nothing, because it only reads the old table. Record
  the last month completed so a killed job resumes.
- The new binary still reads and writes `usage_event`. Nothing user-visible
  changes in release A; it can sit for as long as needed.

**Release B (swap).** One short transaction: `ALTER TABLE usage_event RENAME
TO usage_event_legacy; ALTER TABLE usage_event_p RENAME TO usage_event; DROP
TRIGGER`. The previous release (A) keeps inserting into the name `usage_event`,
which is now the partitioned table; its untargeted `ON CONFLICT DO NOTHING`
is valid against a partitioned table, and it does not know about the identity
table, so its writes during the overlap window bypass the arbiter. That window
is the same thirty minutes every other migration accepts, and a duplicate
there is a retried write for the same request within the window, which today
is rare enough that `0003` accepted the mirror-image loss for a release. The B
binary's `record_usage` goes through the identity table. A final
`INSERT ... SELECT` from `usage_event_legacy` for the last day, `ON CONFLICT DO
NOTHING`, closes the gap between the last backfill and the swap.

**Release C (contract).** Drop `usage_event_legacy`. Only after B has been
live everywhere for longer than any rollback horizon: a rollback to A after C
would leave A inserting into the partitioned table without the trigger, which
still works, but a rollback to anything before A would find no `usage_event`
that release recognises. That is the same constraint the doc on expand/contract
already states; it is just longer here because there are three releases.

Each release bumps `EXPECTED_MIGRATIONS` in `doctor.rs`, and the two tests that
pin `PRIMARY KEY (request_id)` are retired in B and replaced by tests that pin
the identity table as the arbiter.

### Retention, if chosen

A scheduled task, monthly, with the policy in config (`ledger.keep_months`,
`0` for keep everything):

1. Confirm the rollup watermark is past the end of the month to drop.
2. Delete the month's ids from `usage_event_id`.
3. `ALTER TABLE usage_event DETACH PARTITION ... CONCURRENTLY` (Postgres 14+;
   every platform here runs 16 or 18), then `DROP TABLE`.

Detach, not drop in place, so a query running against the partition finishes.
Log the row count and the dollar total dropped, because a retention pass is
the one operation in this system that makes money disappear from the raw
record on purpose, and the rollup's month total is the only thing left to
reconcile it against.

## Recommendation

Do step 1 now; it is small and it was an audit finding (F67) the remediation
left behind. Do step 2 next; it is the change that makes the dashboard's cost
independent of the ledger's size, it needs no migration of the ledger itself,
and the reconcile that just shipped is its template. Then decide retention.
Only if the answer is "drop old months" is step 3 worth its three-release
migration; and if it is, the identity table should land in release A even if
the swap is deferred, because it is also the missing half of the `0003`
contract and worth having on its own.

## Not decided here

- The retention policy itself, which is a product and compliance decision.
- Whether imported rows belong in `usage_daily` or stay ledger-only.
- The rollup's dimension set. The five above are what today's dashboard
  groups by; adding `model_id` multiplies the row count by the catalog size
  and is the first thing someone will ask for. Cheap to add at creation,
  expensive to backfill later.
