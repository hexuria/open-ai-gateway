-- The scheduler's candidate index, without the column that made every
-- request's write to it a non-HOT update.
--
-- `account_schedulable_idx` was (provider, priority, last_used_at) WHERE
-- schedulable. No query orders or filters by `last_used_at` — the scheduler's
-- recency tie-break happens in memory over the candidates it already loaded —
-- so the third column served nothing. What it did was make `last_used_at` an
-- indexed column, so every stamp of it (once per request, on the response
-- path, until that write moved into the ledger statement) was a full tuple
-- rewrite plus an index update rather than a heap-only one. On the account
-- table, which is small and hot, that is bloat for no reader.
DROP INDEX IF EXISTS account_schedulable_idx;
CREATE INDEX account_schedulable_idx
    ON account (provider, priority)
    WHERE schedulable;
